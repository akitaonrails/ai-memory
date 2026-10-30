# Compatibility

## Server versions

| SDK | Minimum server | Newest tested server |
|---|---|---|
| 0.1.x | 3.0.0 | the server built from the same commit |

3.0.0 is the floor because it added conditional page writes and deletes
(`expected_page_id`, `create_only`) and the version id on page reads. An older
server would accept `expectedPageId` as an unknown argument and ignore it,
turning a guarded write into an unconditional one.

The SDK does not compare version strings. On the first wrapped call it reads
the server's `tools/list` and refuses any call whose tool or argument the
server does not list, with `UnsupportedServerError`. A development build
reports what it serves.

Some tools exist only behind a server feature flag. `memory_processing_job_start`
and `memory_processing_job_status` (`jobs.start`, `jobs.status`) are listed only
with `[chunking] enabled = true`, except that a server restarted with the flag
off keeps listing the read-only status tool while jobs from an earlier enabled
run are still stored, and never the start tool. The SDK treats their absence as
a feature gate, not an old server, and says so in `UnsupportedServerError`. The
committed snapshots cover all three configurations: `schema/tools.json` (fresh
flag off, the 25 tools older SDKs know), `schema/tools.chunking.json` (flag on,
27) and `schema/tools.chunking-retained.json` (flag off over existing jobs, 26).

CI runs the integration suite, the schema drift test and every example against
the server built from the same commit, so the newest tested server is always
the current branch.

`GET /identity` and incremental `/api/v1/.../recent` shipped in 2.6.0, so they
are covered by the 3.0.0 floor.

## MCP protocol versions

The client offers `2025-03-26` in `initialize` and accepts `2025-03-26` or
`2024-11-05` in the answer. It uses only `initialize`, `tools/list` and
`tools/call`, which behave the same in both. Stateless mode (the server
default) and stateful mode (`--http-stateful`) are both supported.

## Runtimes

| Runtime | Status |
|---|---|
| Node 22 | supported, tested in CI |
| Node 24 | supported, tested in CI |
| Bun, Deno | not tested in CI; the SDK uses only `fetch`, `AbortSignal`, `TextDecoder` and `URL`. A manual smoke run (write, conditional write, `update`, query, `api.recent`, handoff, admin dry run, transport error) passed on Bun 1.4.3 and Deno 2.9.6 |
| Browsers | not supported: keep tokens on a server |

On a runtime without a global `fetch`, pass one with the `fetch` option.

## Versioning

The SDK versions independently of the server and stays below 1.0.0 until a
caller outside this repository ships on it. During 0.x, a minor release may
add methods or raise the minimum server version; a patch release only fixes
bugs. See [CHANGELOG.md](../CHANGELOG.md).
