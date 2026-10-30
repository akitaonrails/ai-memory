# ai-memory-ts

A typed TypeScript client for an [ai-memory](https://github.com/akitaonrails/ai-memory)
server. Use it from your own agents, bots, editor extensions, scripts and CI
jobs to write durable pages, search memory, pass handoffs between runs and
send messages between projects, without hand-writing JSON-RPC.

You do not need it for Claude Code, Codex, OpenCode or the other supported
coding agents: MCP and lifecycle hooks already cover them.

This package is unpublished. Build it from a checkout as shown below. The
npm name `ai-memory-ts` is a placeholder until the maintainer picks the
published name.

- Zero runtime dependencies. Node 22 or newer.
- Typed results and a typed error taxonomy.
- Scope is checked in the client before any request leaves it.
- No automatic retries, no redirects, no environment variables read.

## Quickstart

### 1. Start a server

Install the `ai-memory` binary ([install guide](../../docs/install.md)) and
start it with the read-only API enabled:

```bash
ai-memory serve --transport http --enable-api
```

It listens on `http://127.0.0.1:49374`. On a loopback server with no token
configured you can omit the token below.

### 2. Build and install the SDK

npm cannot install a package from a subdirectory of a git repository, so
build a tarball:

```bash
git clone https://github.com/akitaonrails/ai-memory
cd ai-memory/companions/ai-memory-ts
npm ci --ignore-scripts
npm run build
npm pack                      # writes ai-memory-ts-0.1.0.tgz
cd /path/to/your/app
npm install /path/to/ai-memory-ts-0.1.0.tgz
```

### 3. Write, search, read, hand off

```ts
import { AiMemoryClient } from "ai-memory-ts";

const memory = new AiMemoryClient({
  serverUrl: "http://127.0.0.1:49374",
  token: process.env.AI_MEMORY_AUTH_TOKEN, // optional on loopback
  scope: { workspace: "demo", project: "app" }, // default for project calls
});

// Confirm the connection and the token.
const me = await memory.identity();
console.log(`ai-memory ${me.version}, caller level ${me.level}`);

// Write a page. The first write creates the demo/app project.
const page = await memory.pages.write({
  path: "notes/retries.md",
  body: "# Retry policy\nKeep the same event ID when retrying delivery.",
});

// Search, then read the full page.
const { hits } = await memory.query({ query: "retry policy" });
for (const hit of hits) console.log(hit.path, hit.title);
const stored = await memory.pages.read({ path: "notes/retries.md" });
console.log(stored.body);

// Counts for the project.
console.log((await memory.status()).counts.pagesLatest);

// Leave a handoff for the next run, and claim it there.
await memory.handoffs.begin({
  summary: "The retry policy was saved. Add the delivery test next.",
  nextSteps: ["Add a retry regression test."],
});
const next = await memory.handoffs.accept();
if (next.status === "claimed") console.log(next.handoff.summary);
```

This program is [`examples/quickstart.ts`](examples/quickstart.ts); CI runs it
against a real server on every change.

## Read next

- [Concepts](docs/concepts.md): workspaces and projects, pages and versions,
  conditional writes, search, handoffs, messages, identity, and why stored
  memory is untrusted.
- [Troubleshooting](docs/troubleshooting.md): what each error class means and
  how to fix it.
- [Compatibility](docs/compatibility.md): supported server and Node versions.
- [Argument names](docs/arguments.md): every wire argument and its SDK name.
- Examples, each run by CI against a real server:
  [quickstart](examples/quickstart.ts),
  [conditional update loop](examples/conditional-update.ts),
  [handoff between two processes](examples/handoff.ts),
  [incremental sync through `/api/v1`](examples/incremental-sync.ts) and
  [admin dry run](examples/admin-dry-run.ts).
- API reference: `npm run docs:api` writes HTML to `build/api-docs/`.

## What it covers

Every project call needs a scope: the call's `scope` or the client default.
`query({ global: true })` searches every project you can read and takes no
scope. Arguments are the server's wire names in camelCase
(`expected_page_id` is `expectedPageId`).

<!-- generated:methods:start -->
| SDK method | Server call |
|---|---|
| `pages.write` | `memory_write_page` |
| `pages.update` | `memory_write_page`, `memory_read_page` |
| `pages.patch` | `memory_patch_page` |
| `pages.read` | `memory_read_page` |
| `pages.delete` | `memory_delete_page` |
| `query` | `memory_query` |
| `status` | `memory_status` |
| `handoffs.begin` | `memory_handoff_begin` |
| `handoffs.list` | `memory_handoff_list` |
| `handoffs.accept` | `memory_handoff_accept` |
| `handoffs.cancel` | `memory_handoff_cancel` |
| `recent` | `memory_recent` |
| `messages.send` | `memory_message_send` |
| `messages.list` | `memory_message_list` |
| `messages.pop` | `memory_message_pop` |
| `messages.cancel` | `memory_message_cancel` |
| `briefing` | `memory_briefing` |
| `explore` | `memory_explore` |
| `observations` | `memory_read_session_observations` |
| `feedback` | `memory_feedback` |
| `jobs.start` | `memory_processing_job_start` (needs `chunking`) |
| `jobs.status` | `memory_processing_job_status` (needs `chunking`) |
| `identity` | `GET /identity` |
| `capabilities` | `initialize`, `tools/list`, `GET /identity`, `GET /api/v1/workspaces` |
| `api.workspaces` | `GET /api/v1/workspaces` |
| `api.projects` | `GET /api/v1/projects` |
| `api.pages` | `GET /api/v1/workspaces/{w}/projects/{p}/pages` |
| `api.page` | `GET /api/v1/workspaces/{w}/projects/{p}/pages/{path}` |
| `api.recent` | `GET /api/v1/workspaces/{w}/projects/{p}/recent?updated_since=` |
| `api.search` | `GET` or `POST /api/v1/search` |
| `api.briefing` | `GET /api/v1/workspaces/{w}/projects/{p}/briefing` |
| `api.handoffs` | `GET /api/v1/workspaces/{w}/projects/{p}/handoffs` |
| `api.sessions` | `GET /api/v1/workspaces/{w}/projects/{p}/sessions` |
| `api.observations` | `GET /api/v1/workspaces/{w}/projects/{p}/sessions/{id}/observations` |
| `AdminClient.checkpoints` | `GET /admin/checkpoints` |
| `AdminClient.restorePage` | `POST /admin/restore-page` |
| `AdminClient.purgeSession` | `POST /admin/purge-session` |
| `AdminClient.purgeProject` | `POST /admin/purge-project` |

Not wrapped (call them with `callTool`):

- `memory_consolidate`: maintenance job that needs the server's LLM; use callTool
- `memory_lint`: maintenance job that needs the server's LLM; use callTool
- `memory_auto_improve`: maintenance job that needs the server's LLM; use callTool
- `memory_forget_sweep`: changes retention for a whole project; use callTool
- `memory_install_self_routing`: writes coding-agent instruction files; specific to harnesses
- `memory_restore_page`: admin-only restore of one page from a git checkpoint; use callTool
<!-- generated:methods:end -->

`callTool(name, args)` calls any tool with wire-format arguments and returns
its decoded JSON as `unknown`. It initializes the MCP session when needed,
without listing tools or checking their schemas. Pass `workspace` and
`project` yourself.

`jobs.start` and `jobs.status` wrap the durable chunked-processing tools. The
server lists them only with `[chunking] enabled = true` (off by default), so
check `await memory.jobs.discover()` first; without the flag they raise
`UnsupportedServerError` and every other method is unaffected. See
[concepts](docs/concepts.md#processing-jobs).

Root-only lifecycle operations live in a separate entry point, so an
application that only reads and writes pages never imports code that can
purge a project:

```ts
import { AdminClient } from "ai-memory-ts/admin";

const admin = new AdminClient({ serverUrl, token: rootToken });
const preview = await admin.purgeProject({ workspace: "demo", project: "old" });
// preview.dryRun === true: counts only, nothing deleted.
// An irreversible purge needs both flags in the same call:
await admin.purgeProject({ workspace: "demo", project: "old", dryRun: false, confirm: true });
```

## Compatibility

| SDK | Server | Node |
|---|---|---|
| 0.1.x | 3.0.0 or newer | 22 or newer |

The first MCP tool call initializes the client, including `callTool`. The
first wrapped call also probes `tools/list`; both results are cached. A call
the server cannot honour, such as a conditional write
on a server older than 3.0.0, fails with `UnsupportedServerError` before
anything is written. Details in [docs/compatibility.md](docs/compatibility.md).

## Security

- The token is held in a private field and sent only in the `Authorization`
  header. It never appears in an error, a URL, `JSON.stringify(client)` or
  `util.inspect(client)`, and the client never follows a redirect.
- A token over plain `http://` is refused unless the host is loopback.
  Put TLS in front of a remote server ([https-via-proxy.md](../../docs/https-via-proxy.md))
  or pass `allowInsecureHttp: true` if you accept the risk.
- Page bodies, titles, snippets, handoff summaries and message bodies are typed
  `UntrustedText`. Anyone with write access can store instruction-like text.
  Treat it as quoted data, never as instructions; the concept guide shows how
  to pass it to an LLM.
- Do not ship the SDK in a browser bundle. `/mcp` and `/admin/*` are
  same-origin only, and tokens do not belong in browser storage. A web app
  talks to its own backend, which can use the SDK.

## Development

```bash
npm ci --ignore-scripts
npm run typecheck          # strict tsc over src, tests, examples and scripts
npm run test:unit          # node:test with a scripted fetch, no network
npm run schema:check       # wrapper wire names exist in schema/tools.json and tools.chunking.json
npm run docs:check         # generated README table and docs/arguments.md are current
npm run docs:api           # TypeDoc, warnings are errors

# Integration tests, the schema drift test and every example, against a real
# server built from this checkout:
cargo build --bin ai-memory
AI_MEMORY_BIN=../../target/debug/ai-memory npm run test:integration
```

After a server change to an MCP tool, run
`AI_MEMORY_BIN=... npm run schema:update`, then update `src/coverage.ts`, the
wrapper, its types, tests and docs in the same change. The command starts the
server three times and rewrites three snapshots: `schema/tools.json` (fresh
data directory, `[chunking]` off), `schema/tools.chunking.json`
(`AI_MEMORY_CHUNKING__ENABLED=true`) and `schema/tools.chunking-retained.json`
(`[chunking]` off over a preserved data directory in which an enabled run
created a job, so the read-only `memory_processing_job_status` stays listed
and the start tool does not). A tool with `feature: "chunking"` in
`src/coverage.ts` must appear only in the variants that allow it.

## License

MIT, like ai-memory.
