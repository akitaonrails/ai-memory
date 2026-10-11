# Design: an optional client SDK (#1165)

**Status: planned, parked (2026-10-10) — implementation waits for the maintainer's go.**

This document plans a typed client library for programs that call ai-memory
over HTTP. Nothing here is implemented. It records the shape the maintainer
would accept (issue #1165, owner comments), the evidence from the repository
behind each choice, and the decisions that remain the maintainer's. It does not
approve any new core endpoint, MCP tool or wire field.

## 1. Goals, non-goals and audience

### Who it is for

People who write programs that use ai-memory without being a coding-agent
harness:

- builders of their own agents and bots who want durable pages, search and
  handoffs without hand-writing JSON-RPC;
- editor and desktop-app extensions, including the marketplace plugins
  proposed in RFC #1166, which need memory tools from TypeScript inside the
  host app;
- automation scripts and CI jobs that write a note, query prior decisions or
  pass a handoff between runs.

Today these callers copy the `curl` and `jq` recipes from
[programmatic-memory.md](programmatic-memory.md), or re-implement what
`companions/ai-memory-wikisync/src/mcp.rs` already does in Rust: build a
JSON-RPC envelope, accept both plain JSON and SSE frames, check JSON-RPC
`error` and `result.isError` separately, decode the tool's JSON from
`result.content[0].text`, and type the precondition failure. Each caller has
to repeat those steps, error handling included.

The companion boundary asks a shared client to justify itself with shipped
callers ([companion-crates.md](companion-crates.md), browser appendix, PR
#1058). The callers named here are the #1165 request and the plugins RFC #1166
proposes. Until one of them ships outside this repository, the SDK stays below
`1.0.0` (§6.1).

### Who it is not for

Users of Claude Code, Codex, OpenCode and the other supported harnesses. MCP
plus lifecycle hooks already give them capture, retrieval and handoffs
([install.md](install.md), [mcp-install.md](mcp-install.md)). The SDK adds
nothing to that path and must never become a requirement for it.

### Goals

1. A small, typed client for the public surfaces that already exist: the MCP
   tools at `/mcp`, the read-only `/api/v1` routes, and `GET /identity`.
2. Typed results and a typed error taxonomy, so callers branch on error
   classes instead of parsing message text.
3. Scope rules enforced in the client before any request leaves it.
4. Documentation that takes a new developer from nothing to a first write and
   query in five minutes.
5. Tests that pin every method's behavior against a real server, so a bug fix
   or a new feature always lands with a test.
6. Fully optional: a separate companion package that no server installer,
   image or package ever contains.

### Non-goals

- No new server surface. The SDK wraps what the server ships. Where it finds a
  gap (an MCP restore tool, a list of deleted pages), that is a separate core
  change with its own permission tests (owner comment on #1165).
- No lifecycle capture. Producers that replace native capture use the
  [external lifecycle contract](external-lifecycle.md) and
  `companions/ai-memory-relay`, which keeps a durable queue the SDK will not
  duplicate.
- No session-aware routing. The SDK is a static MCP client: project calls
  always carry explicit scope (§3.2).
- No browser bundle. `/mcp` and `/admin/*` stay same-origin
  ([frontend-api.md §9](frontend-api.md#9-cors)), and tokens must not live in
  browser storage ([frontend-api.md §2](frontend-api.md#2-auth-model)). A web
  app talks to its own backend, which may use the SDK.
- No LLM calls, no retries that hide failures, no local cache or persistence.

## 2. Packaging and placement

### 2.1 Where it lives

| Package | Path | Phase |
|---|---|---|
| TypeScript client | `companions/ai-memory-ts/` | first |
| Python client | `companions/ai-memory-py/` | later, separate decision |

The `-ts` suffix avoids a clash with `companions/ai-memory-client`, which
already exists: it is the private Rust crate of capture-privacy helpers shared
by the relay and importer (`companions/ai-memory-client/README.md`).

Each package follows the companion boundary in
[companion-crates.md](companion-crates.md): its own manifest (`package.json`
with a committed `package-lock.json`; later `pyproject.toml`), its own tests,
README and changelog, and it talks to ai-memory only through public HTTP and
MCP surfaces. It never opens the wiki directory or SQLite.

Proposed layout of the first package:

```text
companions/ai-memory-ts/
├── package.json            # "private": true until publishing is approved
├── package-lock.json
├── tsconfig.json
├── typedoc.json
├── README.md               # quickstart and links
├── CHANGELOG.md
├── docs/                   # concept guide, troubleshooting, compatibility
├── src/
│   ├── index.ts            # public exports only
│   ├── client.ts           # AiMemoryClient and namespaces
│   ├── transport.ts        # fetch, JSON-RPC, SSE frames, timeouts
│   ├── errors.ts           # error taxonomy
│   ├── scope.ts            # scope validation
│   ├── capabilities.ts     # initialize, tools/list, /identity probing
│   ├── api.ts              # /api/v1 reads and pagination
│   └── types.ts            # argument and result types
├── schema/tools.json       # snapshot of the server's tool input schemas
├── examples/               # runnable, tested in CI
└── test/
    ├── unit/
    └── integration/
```

### 2.2 Never part of the server distribution

The server's distribution channels are built from the root Cargo workspace and
the `hooks/` tree, and the SDK must stay out of all of them:

- Release tarballs copy the binary and `hooks/` only
  (`.github/workflows/release.yml`, the `cp -a hooks` steps). Homebrew installs
  those tarballs.
- The Docker image copies `crates`, `evals`, `docs` and `hooks`
  (`docker/Dockerfile`), never `companions/`.
- The AUR packages build the root workspace and copy `hooks`
  (`packaging/aur/PKGBUILD`, `PKGBUILD-bin`). The source archive they download
  contains every tracked file, so the directory is present in the build tree
  but nothing builds or installs it.
- The Nix flake uses `src = ./.` (`flake.nix`) and builds the root workspace.
  Git-tracked files only enter the source, so `node_modules/` and `dist/` must
  stay gitignored inside the companion.
- The root `Cargo.toml` workspace members do not include companions, and the
  SDK has no Cargo manifest at all.

The first slice adds a guard so this stays true: a check in
`scripts/check-native-packaging.sh` (or a CLI repo-layout test, matching the
existing one that refuses stray test files) that fails if `release.yml`, the
Dockerfile, a PKGBUILD or the flake references `companions/ai-memory-ts`.

### 2.3 Installing it

Before publishing is approved, a developer builds the package from a checkout.
npm cannot install a package from a subdirectory of a git repository, so the
supported path is a local tarball:

```bash
git clone https://github.com/akitaonrails/ai-memory
cd ai-memory/companions/ai-memory-ts
npm ci --ignore-scripts
npm run build
npm pack                      # writes <package-name>-<version>.tgz
cd /path/to/your/app
npm install /path/to/<package-name>-<version>.tgz
```

The README states that this build is unpublished and unsupported outside the
documented compatibility range.

If the maintainer approves publishing (§8, decision D1), the package goes to
npm under a name the maintainer picks, from a dedicated GitHub Actions workflow
using npm trusted publishing with provenance, and installs with
`npm install <name>`. That workflow is separate from the server's
`release.yml` and runs on its own tag prefix (for example `sdk-ts-v0.3.0`), so
a server release never publishes the SDK and the SDK never delays a server
release.

## 3. API design

### 3.1 Client surface

One entry class with namespaces that mirror the server's concepts. The first
release targets Node 22 and newer and also runs on Deno and Bun through
standard `fetch`; callers on other runtimes inject their own `fetch`.

```ts
import { AiMemoryClient } from "ai-memory-ts"; // final name: decision D1

const memory = new AiMemoryClient({
  serverUrl: "http://127.0.0.1:49374",
  token: process.env.AI_MEMORY_AUTH_TOKEN,      // optional on loopback
  scope: { workspace: "demo", project: "app" }, // default for project calls
  timeoutMs: 30_000,
});

const page = await memory.pages.write({
  path: "notes/retries.md",
  body: "# Retry policy\nKeep the same event ID when retrying delivery.",
});
const result = await memory.query({ query: "retry policy" });
for (const hit of result.hits) console.log(hit.path, hit.title);
```

| Namespace | Methods | Server call |
|---|---|---|
| `pages` | `write`, `read`, `delete` | `memory_write_page`, `memory_read_page`, `memory_delete_page` |
| (root) | `query`, `recent`, `status` | `memory_query`, `memory_recent`, `memory_status` |
| `handoffs` | `begin`, `list`, `accept`, `cancel` | `memory_handoff_*` |
| `messages` | `send`, `list`, `pop`, `cancel` | `memory_message_*` |
| `api` | `workspaces`, `projects`, `pages`, `page`, `recent`, `search`, `briefing`, `handoffs`, `sessions`, `observations` | `/api/v1/*` ([frontend-api.md §4](frontend-api.md#4-endpoint-reference)) |
| (root) | `capabilities`, `identity` | `initialize`, `tools/list`, `GET /identity` |

Every server tool is classified, and the drift test in §5.3 fails when a new
tool appears without a row here:

| Tool | SDK | Reason |
|---|---|---|
| `memory_write_page`, `memory_read_page`, `memory_delete_page`, `memory_query` | phase 1 | core page workflow |
| `memory_handoff_begin`, `_list`, `_accept`, `_cancel` | phase 1 | continuity between runs |
| `memory_status` | phase 1 | health check in the quickstart |
| `memory_recent`, `memory_message_send`, `_list`, `_pop`, `_cancel` | phase 2 | messaging and recency |
| `memory_briefing`, `memory_explore`, `memory_read_session_observations`, `memory_feedback` | phase 2 | reads for agents built on the SDK |
| `memory_consolidate`, `memory_lint`, `memory_auto_improve`, `memory_forget_sweep` | not exposed | maintenance jobs that need an LLM or change retention; callers can use `callTool` |
| `memory_install_self_routing` | not exposed | writes harness instruction files; specific to coding agents |

`memory.callTool(name, args)` stays available as a typed escape hatch that
returns the decoded JSON as `unknown`, for tools without a wrapper and for
servers newer than the SDK.

Argument names are the wire names in camelCase (`expected_page_id` becomes
`expectedPageId`). The mapping is mechanical, and a table generated from the
schema snapshot appears in the API reference.

#### `/api/v1` reads and pagination

`/api/v1` requires `serve --enable-api` or `--enable-web`
([programmatic-memory.md](programmatic-memory.md#read-changed-pages)).
`api.recent({ updatedSince })` returns an async iterator that follows
`next_cursor` until it is `null`, with a bound on rounds so a hostile server
cannot loop the client (wikisync uses 1,000 rounds, `src/client.rs`
`MAX_LIST_ROUNDS`). `api.page(path, { ifNoneMatch })` exposes the page `ETag`
and returns `{ notModified: true }` on `304`. `api.page` returns `id`, the
version token for conditional writes ([frontend-api.md
§4.4](frontend-api.md#44-page-read-full)). Limits clamp server-side to
`1..=100` (`1..=200` for handoff history and observations); the SDK passes the
caller's value through and documents the clamp.

#### Admin operations: a separate, opt-in `AdminClient`

Restore and purge are root-only admin routes, not MCP tools
([programmatic-memory.md, Lifecycle from code](programmatic-memory.md#lifecycle-from-code)).
They live in a separate class, imported from a separate entry point
(`ai-memory-ts/admin`), so an application that only reads and writes pages
never imports code that can purge a project:

```ts
import { AdminClient } from "ai-memory-ts/admin";

const admin = new AdminClient({ serverUrl, token: process.env.ROOT_TOKEN });
const preview = await admin.purgeSession({ workspace, project, sessionId });
// preview.dryRun === true: counts only, nothing deleted
await admin.purgeSession({ workspace, project, sessionId, dryRun: false, confirm: true });
```

- `purgeSession` and `purgeProject` default to `dryRun: true`. An
  irreversible purge needs both `dryRun: false` and `confirm: true` in the same
  call. The server already lets `dry_run` win over `confirm` and refuses with
  400 when `confirm` is not `true`; the client mirrors that and refuses before
  sending.
- `restorePage({ workspace, project, path, rev })` and
  `checkpoints({ limit })` cover the documented restore flow. The server's
  restore has no dry run (`RestorePageRequest` in
  `crates/ai-memory-mcp/src/admin.rs` has no such field), so the method writes
  a new latest version and its documentation says so.
- `force` on `purgeProject` is a separate flag with its own warning, because
  it purges despite a live managed-workstream lease.
- A 403 from an admin route raises `ForbiddenError` with a hint that `/admin/*`
  is root-only once DB users exist, and that a user `aim_` key is always
  user-level ([users.md](users.md#the-four-resolution-rungs)).

### 3.2 Scope rules

The SDK is a static MCP client, so it follows the static-client rule in
`AGENTS.md` and the server's `MEMORY_INSTRUCTIONS`:

- Project-scoped calls send `workspace` and `project` together, every time.
  The type is `Scope = { workspace: string; project: string }`; both fields are
  required, and a runtime check rejects an empty or one-sided scope with
  `ScopeError` before any request.
- Scope comes from the call's `scope` argument, or the client's default. With
  neither, the call fails with `ScopeError`. The SDK never relies on the
  server's active-project pointer, never derives a project from the working
  directory, and never sends `cwd`.
- A global query (`query({ query, global: true })`) omits `workspace`,
  `project` and `scopes`. The argument type is a discriminated union, so
  passing a scope with `global: true` is a compile error, and the runtime check
  catches untyped callers.
- Multi-project queries pass `scopes: Scope[]`, each element validated the same
  way.
- A later helper may read workspace and project from the nearest
  `.ai-memory.toml` for scripts that run inside a repository, but only when
  the marker declares both. That stays out of the first slice (open question
  Q3).

Writes and handoff creation may create a missing explicit scope; reads fail
closed on a missing scope ([programmatic-memory.md](programmatic-memory.md)).
The SDK surfaces those server errors as typed errors and adds no fallback.

### 3.3 Typed results

MCP tool results carry their payload as JSON text in `result.content[].text`,
and the server's tools publish no output schema (the responses are built
ad hoc in `crates/ai-memory-mcp/src/server.rs`; protocol `2024-11-05`, set at
`get_info`, predates `outputSchema`). The SDK therefore hand-writes result
types for each wrapped tool, decodes the JSON, checks the fields it promises,
and passes unknown extra fields through unchanged so a newer server does not
break an older client. The integration suite (§5.2) pins every promised field
against a real server; that is the contract.

Results that carry stored memory use a branded type:

```ts
declare const untrusted: unique symbol;
export type UntrustedText = string & { readonly [untrusted]: true };
```

Page bodies, titles, hit snippets, handoff summaries and message bodies are
`UntrustedText`. It is still a string and reads like one. The type name and its
documentation remind the caller that the content is historical data to quote,
never instructions to follow (§7).

Examples of result types:

- `pages.write` returns `{ pageId, path }`; `pages.delete` returns
  `{ path, deleted, preCheckpoint, checkpoint }`.
- `handoffs.accept` returns a union on `status`: `claimed` with the handoff,
  `consumed_by_hook`, or `none_pending` (`HandoffAcceptStatus` in
  `server.rs`).
- `query` returns `{ hits }`, where a superseded version has
  `superseded: true` when `includeSuperseded` was set.

### 3.4 Error taxonomy

All errors extend `AiMemoryError`, which carries a stable `code` string for
callers who prefer a switch over `instanceof`.

| Class | When | Notable fields |
|---|---|---|
| `ConfigError` | invalid server URL, URL with credentials, query or fragment; token over plain HTTP to a non-loopback host without `allowInsecureHttp` | none |
| `ScopeError` | missing, partial or empty scope; scope combined with `global` | none |
| `TransportError` | DNS, connection refused, TLS, refused redirect | `cause` (sanitized) |
| `TimeoutError` | the client's own deadline expired | `uncertain` |
| `AbortError` | the caller's `AbortSignal` fired | `uncertain` |
| `HttpError` | non-2xx HTTP status | `status`, bounded `preview` |
| `AuthError` (401), `ForbiddenError` (403), `NotFoundError` (404), `RateLimitedError` (429) | subclasses of `HttpError` | `hint` |
| `RpcError` | JSON-RPC `error` member | `rpcCode`, bounded `message`, `data` |
| `InvalidParamsError` | `RpcError` with `-32602` | none |
| `PreconditionFailedError` | `RpcError` with `-32600` and `data.reason == "precondition_failed"` | `path`, `expectedPageId`, `currentPageId` |
| `ToolError` | `result.isError == true` | bounded `detail` |
| `ProtocolError` | no JSON-RPC message in the body, missing `result`, tool JSON that does not decode, a result missing a promised field | bounded `preview` |
| `UnsupportedServerError` | the server lacks a tool, an argument or an endpoint the call needs | `feature`, `serverVersion`, `minimumVersion` |

The precondition decoding matches wikisync's `precondition_failure` exactly:
code `-32600` and reason `precondition_failed` together, and nothing else
(`companions/ai-memory-wikisync/src/mcp.rs`). An admission-webhook rejection
arrives as `-32603` ([programmatic-memory.md](programmatic-memory.md)), which
is also the generic internal-error code, so it stays a plain `RpcError` until
the server marks it with a `data.reason` of its own (open question Q5).

`uncertain: true` marks a mutating call (write, delete, handoff begin, accept
or cancel, message send, pop or cancel, any admin call) that timed out or was
aborted after the request was sent. The server may or may not have applied it.
The troubleshooting guide shows how to reconcile: re-read the page and compare
its `id`, or list handoffs, before retrying. A handoff is claimed once, so a
blind retry of `accept` can return `none_pending` or the next eligible handoff,
and the first claim goes unnoticed.

Every server-supplied string in an error goes through one `preview` function:
at most 300 characters, control characters replaced, the same bounds as
wikisync's `ERROR_PREVIEW`. An error never contains the token, a request
header or a request body.

### 3.5 Transport, timeouts and cancellation

- `POST {serverUrl}/mcp` with
  `Accept: application/json, text/event-stream`. The response is either plain
  JSON (default stateless mode) or SSE; the SDK takes the last `data:` frame
  that parses as JSON, as `json_rpc_message` does in wikisync.
- Stateless is the default and needs no handshake. If `initialize` returns an
  `Mcp-Session-Id` (a server started with `--http-stateful`), the client keeps
  it, sends `notifications/initialized`, and attaches the header to later
  requests.
- No `?flavor=` marker. The server serves the upstream schema dialect unless
  the operator configured a stricter floor (`list_tools` in `server.rs`), so
  feature probing checks property presence and tolerates rewritten schemas.
- No `X-Memory-Actor-*` headers. Those belong to the trusted-proxy rung
  ([users.md](users.md)); the server ignores them on the root rung, and an SDK
  that sent them would invite misuse.
- `redirect: "error"`, so a bearer token never follows a redirect to another
  origin. The relay refuses redirects the same way
  (`tests/e2e/external_relay_smoke.py`, "redirect refused").
- Default deadline 30 seconds per request (wikisync's `REQUEST_TIMEOUT`),
  configurable per client and per call. A caller `signal` combines with the
  deadline through `AbortSignal.any`, and the SDK reports which one fired. The
  generated Pi bridge does the same (`mcpSignal` in
  `crates/ai-memory-cli/src/commands/install_hooks.rs`).
- No automatic retries. Reads are safe to retry and callers can do it in one
  line; writes are not, and silent retries would turn an uncertain outcome
  into a duplicate or a lost claim.
- JSON-RPC ids increase per client instance.

### 3.6 Protocol version and server compatibility

- The client sends `initialize` with the newest MCP protocol version it
  implements and accepts the server's answer when it is in the SDK's supported
  list. ai-memory answers `2024-11-05` today (`get_info` in `server.rs`); the
  Pi bridge already sends `2025-03-26` and accepts that answer. The SDK uses
  only `initialize`, `tools/list` and `tools/call`, which behave the same across
  those versions.
- The minimum server is 2.7.0, the release that adds conditional writes
  and deletes (`expected_page_id`, `create_only`) and the version id on page
  reads (`CHANGELOG.md`, `[Unreleased]` on `release/2.7`). `GET /identity` and
  incremental `recent` shipped in 2.6.0. A floor below the precondition seam
  would let an older server ignore `expectedPageId` as an unknown argument and
  turn a guarded write into an unconditional one, which is why wikisync
  refuses servers older than 2.7 (`ensure_conditional_writes` in
  `companions/ai-memory-wikisync/src/mcp.rs`).
- `capabilities()` runs `initialize` (server name, version, protocol version),
  `tools/list` (tool names and input-schema property names) and
  `GET /identity` (`level`, `operator`, `version`), and probes whether `/api/v1`
  is mounted. It caches the result per client instance; `capabilities({
  refresh: true })` re-probes.
- Each wrapped method declares what it needs: a tool name, plus any argument
  whose absence would change meaning. Before the first call that needs a
  feature, the client checks the cached `tools/list`. A missing tool or
  argument raises `UnsupportedServerError` naming the feature and the minimum
  version, before anything is written. Probing reads the schema, so a
  development build reports what it serves. The version string appears only in
  error messages and the compatibility check in §6.

### 3.7 No runtime dependencies

The package has zero runtime dependencies: `fetch`, `AbortSignal`,
`TextDecoder` and `URL` cover the transport. Development dependencies are
limited to `typescript`, `@types/node` and `typedoc`; tests use the built-in
`node:test` runner. Adding any dependency, runtime or development, needs a
reason in the pull request, the same policy the root workspace applies to
crates (`AGENTS.md`, dependency policy).

## 4. Documentation plan

The audience is a developer who has never seen ai-memory. Every page in this
section lives in `companions/ai-memory-ts/` and ships in the package.

### 4.1 README quickstart

A five-minute path, tested in CI as an example (§4.4):

1. Install and start a local server: `ai-memory serve --transport http
   --enable-api` (link to [install.md](install.md) for the binary).
2. Install the SDK (§2.3).
3. Create a client with a `demo/app` scope.
4. `status()` to confirm the connection.
5. `pages.write`, then `query`, then `pages.read`, printing the result.
6. `handoffs.begin` and `handoffs.accept` to show continuity between two runs.

Below the quickstart: what to read next, the compatibility table, and a short
security section on tokens and untrusted memory.

### 4.2 Concept guide

`docs/concepts.md` explains the following, linking to the server docs instead
of copying them:

- workspaces and projects, and why a static client always passes both
  ([auto-scope.md](auto-scope.md), [marker-file.md](marker-file.md));
- pages: paths, versions, supersession, replace semantics (a write clears
  omitted metadata), conditional writes with `expectedPageId` and
  `createOnly`;
- query: project, multi-scope and global search, superseded and expired
  versions;
- handoffs: one claim per handoff, ownership, `anyOwner`
  ([usage.md](usage.md));
- messages between projects and their access rules
  ([agent-messaging.md](agent-messaging.md), [users.md](users.md#per-project-access));
- identity and auth: root token, `aim_` user keys, `/identity`;
- retrieved memory is untrusted data, with a worked example of passing a page
  to an LLM as quoted context.

### 4.3 API reference

TypeDoc generates the reference from the TSDoc comments in `src/`. Every
exported symbol needs a comment; TypeDoc runs with warnings treated as errors,
so a missing comment fails CI. Each method's comment names the server tool or
route it calls, the errors it raises and the minimum server version. The
generated HTML is a build artifact, not a committed file, until publishing
gives it a home (decision D1).

### 4.4 Examples

`examples/` holds small runnable programs: the quickstart, a conditional
update loop that handles `PreconditionFailedError`, a handoff between two
processes, incremental sync with `api.recent`, and (once phase 3 lands) an
admin dry run. Each example takes its server URL and token from the
environment, and the integration job runs every one against the CI server
(§5.5).

### 4.5 Troubleshooting

`docs/troubleshooting.md` maps each error class to causes and fixes:

- `AuthError`: missing or wrong token; the static root token versus `aim_`
  keys; a bearer never authenticates `/auth/*`.
- `ForbiddenError`: restricted project without a grant; admin route with a
  user key; `Host` not in `AI_MEMORY_ALLOWED_HOSTS`.
- `ScopeError`: the call had no scope, or only half of one.
- A server error on the first read of a new project: reads never create a
  scope, so write first or check the names.
- `UnsupportedServerError`: how to read the server version and upgrade.
- `NotFoundError` from every `/api/v1` route: the server probably runs
  without `--enable-api`; `capabilities()` reports whether the API is
  mounted.
- `uncertain` timeouts: how to reconcile before retrying.
- `ConfigError` on plain HTTP to a remote host: put TLS in front
  ([https-via-proxy.md](https-via-proxy.md)).

### 4.6 Keeping docs in sync

- The method-to-tool table in the README is generated from the same mapping
  the drift test reads, and CI fails when the committed copy differs.
- A server change that alters a wrapped tool updates the SDK's schema
  snapshot, types, docs and tests in the same pull request (§6.3).
- [programmatic-memory.md](programmatic-memory.md) gains a short "Use the SDK"
  pointer once the package exists, and the curl recipes stay as the
  language-neutral reference.

## 5. Testing strategy

Two rules come first and apply from the first commit:

- Every SDK bug fix lands with a regression test that fails without the fix.
- Every server tool or feature exposed through the SDK lands with unit tests,
  integration tests against a real server, and docs, in the same change.

### 5.1 Unit tests

`node:test` with an injected `fetch` that records requests and returns scripted
responses. No network. Coverage:

- request shape per method: URL, headers, JSON-RPC envelope, argument names,
  omitted optional arguments;
- scope validation: missing, partial, empty, and scope with `global`, each
  refused before `fetch` is called (asserted by the recorder staying empty);
- response parsing: plain JSON, SSE with several frames, SSE with no data
  frame, malformed JSON;
- every error class from scripted responses, including the precondition
  decoding table from wikisync's `precondition_errors_are_typed` (right code
  and reason, wrong reason, wrong code, missing data);
- timeouts and caller aborts, and the `uncertain` flag on mutations versus
  reads;
- pagination: cursor following, the round bound, an immediate `null` cursor;
- capability gating: a scripted `tools/list` without `expected_page_id` makes
  a conditional write raise `UnsupportedServerError` with no `tools/call` sent.

### 5.2 Integration tests against a real server

The harness spawns the `ai-memory` binary built earlier in the same CI job,
following `tests/e2e/external_relay_smoke.py`: a temporary data directory, a
free loopback port, `serve --transport http --enable-api --no-watcher`, a
synthetic root token in `AI_MEMORY_AUTH_TOKEN`, and a readiness loop with a
deadline. Setup creates a DB user and an `aim_` key through `ai-memory user
add-human` and `ai-memory api-key add` ([users.md](users.md)), so tests run at
both root and user level. Each test uses its own workspace and project.

Every public method has at least one success test and its failure paths:

- write, read, query, delete; `create_only` on an existing page and a stale
  `expectedPageId` raise `PreconditionFailedError` with the right ids, and the
  page is unchanged afterwards;
- a read of a missing scope fails closed;
- handoff begin, list, accept by id, accept latest, a second accept returns
  `none_pending`, cancel;
- messages between two projects, including a restricted target without a
  grant (refused) and with a grant (control);
- `/api/v1` reads with pagination over more pages than one `limit`, the ETag
  `304` path, and `NotFoundError` from a server without `--enable-api`;
- auth: wrong token (`AuthError`), user key on an admin route
  (`ForbiddenError`), and the root-token control;
- result contracts: every field the types promise is present with the right
  type for each wrapped tool.

### 5.3 Schema drift test

`schema/tools.json` is a committed snapshot of the server's `tools/list`,
normalized: sorted keys, descriptions removed (they change often and carry no
contract), keeping tool names, property names, types, `required` and enums.
The integration job fetches `tools/list` from the CI server and compares:

- a changed input schema for a wrapped tool fails, naming the tool and the
  differing properties;
- a new tool fails until it is classified in the coverage table (§3.1) as
  wrapped or not exposed;
- a removed tool fails.

The failure message tells the developer to run `npm run schema:update`, then
update the method, its types, tests and docs. A second, static check confirms
that every argument a wrapper can send exists in the snapshot, so a typo in a
wire name fails without a server.

### 5.4 Security tests

Adversarial, each with a legitimate control, in the style
[security-boundaries.md](security-boundaries.md) asks for:

- the token never appears in any error's `message`, `stack`, `cause`, in
  `JSON.stringify(client)` or `util.inspect(client)`, or in a URL. The test uses
  a distinctive synthetic token and greps every surface. The client stores it
  in a private class field and redacts it in `toJSON` and the inspect hook;
- server URLs with credentials, a query or a fragment raise `ConfigError`;
- a token sent over plain HTTP to a non-loopback host is refused without
  `allowInsecureHttp`; loopback is the control;
- a redirect response raises `TransportError` and the redirect target receives
  no request;
- a hostile server (a scripted `fetch` in unit tests) returning megabyte
  error bodies and terminal escape sequences produces previews of at most 300
  characters with control characters replaced;
- untrusted memory: a page whose body contains instruction-like text and
  fake JSON-RPC frames comes back as the server stored it, as plain data; the
  SDK makes no extra request (no feedback call, no link fetch) because of
  content;
- admin: a purge without `confirm: true` and `dryRun: false` never reaches the
  wire; the dry-run default returns counts and deletes nothing (checked with a
  read afterwards).

### 5.5 CI job design

Two pieces, matching existing patterns in `.github/workflows/ci.yml`:

1. A standalone `sdk-ts` job on `ubuntu-latest`, no Rust: pinned
   `actions/checkout` and `actions/setup-node` by commit SHA, as every job in
   `ci.yml` already does; Node 22 and 24 in a matrix; `npm ci --ignore-scripts`;
   type check in strict mode; unit tests; the static schema check; TypeDoc with
   warnings as errors; the generated-docs freshness check; `npm audit
   --audit-level=moderate` against the lockfile.
2. Integration steps added to the Linux leg of the existing `test` job, after
   the relay smoke, reusing `target/debug/ai-memory` the way the relay smoke
   does (`if: matrix.os == 'ubuntu-latest'`, Node already set up there):
   integration tests, the drift test and every example.

Because the second piece runs on every pull request that builds the server, a
change to an MCP tool schema fails CI until the SDK snapshot is updated, even
when the pull request never touches `companions/`. Windows and macOS legs are
not needed: the SDK has no platform-specific code, and the server's own
matrix covers the server.

## 6. Versioning and release

### 6.1 Independent semver

The SDK starts at `0.1.0` and versions independently of the server. Patch:
fixes. Minor: new wrapped tools, new options, a raised minimum server version
during `0.x`. Major (after `1.0.0`): removed methods, changed result types, a
raised minimum server version. `1.0.0` waits for phase 2 and at least one
shipped caller outside this repository.

### 6.2 Compatibility matrix and changelog

`docs/compatibility.md` in the package lists, per SDK minor, the minimum and
the newest tested server version. The integration job always tests against
the server built from the same commit, so the newest tested version is the
current branch. `CHANGELOG.md` in the package follows Keep a Changelog, like
the root changelog. SDK changes do not add entries to the root `CHANGELOG.md`
unless they also change the server.

### 6.3 How server changes reach the SDK

`AGENTS.md` already says that MCP tool surface changes update
`MEMORY_INSTRUCTIONS`, `SNIPPET_BODY`, docs and the prompt-surface tests. Once
the SDK exists, the first slice extends that rule:

> MCP tool surface changes also update the TypeScript SDK in
> `companions/ai-memory-ts` (schema snapshot, wrapper or coverage-table
> classification, types, tests and docs) in the same change.

The drift test (§5.3) enforces it. The same applies to `/api/v1` changes for
the endpoints the SDK wraps, enforced by the integration tests.

## 7. Security considerations

- The token is held in a private field and sent only in the `Authorization`
  header. It is never logged or forwarded across a redirect, and never appears
  in a URL, an error or serialized output. The SDK reads no environment
  variables itself: the caller passes the token, so the caller's code shows
  where the secret comes from.
- Plain HTTP with a token is allowed to loopback only, unless the caller opts
  in. TLS belongs to a reverse proxy ([https-via-proxy.md](https-via-proxy.md)).
- Everything the server returns from memory is historical data that may
  contain instruction-like text, the same rule `MEMORY_INSTRUCTIONS` gives
  agents. The SDK never executes, renders or follows stored content, and its
  docs show how to pass it to an LLM as quoted context. The `UntrustedText`
  brand carries the reminder into the caller's types.
- Scope validation in the SDK is a convenience. The server's scope resolution,
  `AuthLevel::authorize`, per-project grants, admission webhooks and
  sanitization remain the boundaries that count, and the SDK uses only public
  routes, so it bypasses none of them.
- Admin methods sit behind a separate entry point. Purges default to dry run,
  and an irreversible call needs two explicit flags.
- Server-supplied text in errors is bounded and stripped of control
  characters, and pagination has a round limit.
- For the supply chain: zero runtime dependencies, a committed lockfile,
  `npm ci --ignore-scripts`, `npm audit` in CI, pinned actions, and provenance
  from trusted publishing if the package is published.
- The SDK adds no server-side boundary, so it needs no row in
  [security-boundaries.md](security-boundaries.md). Any core change it
  motivates (decision D4) needs its own row and adversarial tests.

## 8. Phased plan

Estimates assume one developer familiar with TypeScript and the repository.

### Phase 1: first slice (about 6 to 8 working days)

Files:

- `companions/ai-memory-ts/` with the layout in §2.1, `"private": true`.
- `src/transport.ts`, `errors.ts`, `scope.ts`, `capabilities.ts`, `client.ts`,
  `types.ts`, `index.ts`.
- Methods: `capabilities`, `identity`, `status`, `pages.write`, `pages.read`,
  `pages.delete`, `query`, `handoffs.begin`, `handoffs.list`,
  `handoffs.accept`, `handoffs.cancel`, `callTool`.
- `schema/tools.json` and `npm run schema:update`.

Tests: the unit suite (§5.1) for every method; integration tests (§5.2) for
every method and its failure paths; the drift test; the security tests in §5.4
that apply to these methods.

CI: the `sdk-ts` job and the integration steps in the `test` job (§5.5); the
packaging guard (§2.2).

Docs: README quickstart, concept guide, troubleshooting, TypeDoc config,
examples for the quickstart and the conditional update loop; the AGENTS.md
rule extension (§6.3); a pointer in [programmatic-memory.md](programmatic-memory.md)
and [companion-crates.md](companion-crates.md).

Maintainer decisions before phase 1 starts: approve the start (this document),
and confirm the 2.7.0 floor.

### Phase 2: reads and messaging (about 4 to 5 days)

`api.*` with pagination and ETag, `recent`, `messages.*`, `briefing`,
`explore`, `observations`, `feedback`; matching tests, examples and docs.
Decision: none beyond phase 1, unless the maintainer wants to trim the list.

### Phase 3: `AdminClient` (about 3 to 4 days)

`checkpoints`, `restorePage`, `purgeSession`, `purgeProject`, behind
`ai-memory-ts/admin`, with the dry-run defaults and the admin security tests.
Decision D3: whether the SDK should ship admin methods at all, or leave
lifecycle operations to the CLI and the documented HTTP recipes.

### Phase 4: publishing (about 1 to 2 days, then ongoing)

The publish workflow with trusted publishing and provenance, the package name,
hosted API reference. Decision D1: publish to npm, under which name, and who
owns the npm account and responds to security reports for the package.
Publishing is a long-term maintenance commitment (owner comment on #1165).

### Phase 5: Python client (about 7 to 10 days)

`companions/ai-memory-py/` with the same surface, standard library only
(`urllib.request`, `json`), the same error taxonomy, sharing the schema
snapshot and the integration harness. Decision D2: build it, and whether to
publish to PyPI.

### Core changes the SDK might motivate

Decision D4, each a separate core change with permission tests and its own
review, never part of an SDK pull request:

- an MCP restore tool, so restore does not need the root admin route;
- a "list deleted pages" endpoint (today the only source is the delete
  response or `/admin/checkpoints`);
- a `data.reason` on admission-webhook rejections, so the SDK can type them;
- output schemas on tool results, which would need a newer MCP protocol
  version on the server.

None is required for phases 1 to 5. Per [companion-crates.md, When to move a
seam into core](companion-crates.md#when-to-move-a-seam-into-core), each should
wait until shipped SDK callers show the need.

## 9. Open questions

- Q1 (package name). The directory is `ai-memory-ts`; the npm name needs
  an availability check and the maintainer's choice (D1).
- Q2 (base paths). wikisync refuses servers behind a path prefix. Should the
  SDK accept `https://host/prefix` for reverse-proxy deployments, and how would
  CI test it?
- Q3 (marker file). Should a helper read `.ai-memory.toml` when both names
  are declared, or should scope always come from the caller?
- Q4 (non-project writes). `memory_write_page` accepts a `scope` of
  `global` or `profile`. Should the SDK expose those writes in phase 2, given
  that they affect every project?
- Q5 (admission rejections). Is a typed `AdmissionRejectedError` worth a
  core change to add `data.reason`?
- Q6 (session attribution). `memory_write_page` accepts `session_id`. Is
  there an SDK caller that needs to attribute writes to a captured
  session, or does that belong to the relay and harness integrations?
- Q7 (read-modify-write helper). A `pages.update(path, change)` helper that
  reads, applies a change and writes with `expectedPageId` would remove a
  common mistake (a write clears omitted metadata). Should it ship in phase 1,
  or wait for a caller?
- Q8 (Node floor). Node 22 is the proposed minimum. Should CI also cover
  Deno and Bun, or document them as untested?
