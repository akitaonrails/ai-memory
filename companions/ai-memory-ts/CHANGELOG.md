# Changelog

All notable changes to the TypeScript SDK are documented here. The SDK
versions independently of the ai-memory server.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Capability initialization is owned by the client rather than its first
  caller. Concurrent callers now wait with independent abort signals and
  deadlines; cancelling one does not fail or delay the others. (#1815)

- Initialized the MCP session before the first `callTool`, so raw tool calls
  work with `--http-stateful` without a preceding wrapped call. Raw calls
  still skip `tools/list` and schema checks. (#1816)

### Added

- `jobs.start` and `jobs.status` wrap the server's durable chunked
  session-processing tools (`memory_processing_job_start` and
  `memory_processing_job_status`), with typed `ProcessingJobStatus`
  (progress, call budget, redacted error, result). The server lists them only
  with `[chunking] enabled = true`; `jobs.discover()` reports whether it does,
  and `UnsupportedServerError` names the flag instead of a server version when
  a call needs a tool the server hides. Existing methods are unchanged.
  The schema snapshots now have three variants: fresh flag off
  (`tools.json`, 25 tools), flag on (`tools.chunking.json`, 27) and flag off
  over a data directory that holds jobs (`tools.chunking-retained.json`, 26,
  the read-only status tool only), and the coverage table classifies the two
  job tools as conditional. `chunksTotal` and `chunksDone` are numbers (`0`
  before planning). `ProcessingJobStatus` also carries `waitMs` and `execMs`:
  cumulative milliseconds of provider-call queue wait and execution.

- Decoded optional query-answer context admission receipts as `answerContext` (#1684).

- `query` takes the server's `memory_query` page filters: `kind`, `tier`,
  `pathPrefix` and `tag`, in every search mode.

- `messages.pop` takes `agent`, the harness claiming harness-addressed mail,
  and `Message` carries `toAgent` (`null` for shared project mail), matching
  the server's new `to_agent` addressing.

- `pages.read` takes `maxChars`, and `Page` carries `truncated` and
  `totalChars`, matching the server's new `memory_read_page` size cap. Both are
  `undefined` against a server that predates it. The documented `observations`
  defaults are now 20 rows of 1000 characters.

## [0.1.0]

### Added

- First release (unpublished; build from a checkout). `AiMemoryClient` wraps
  the page, query, status, handoff, recent, message, briefing, explore,
  session-observation and feedback tools, plus the read-only `/api/v1` routes
  with cursor paging and `ETag` revalidation, `GET /identity` and capability
  probing. `pages.update` is a read-modify-write helper guarded by
  `expectedPageId`. `AdminClient` (from `ai-memory-ts/admin`) lists
  checkpoints, restores pages and purges sessions or projects, with dry runs
  by default. Requires ai-memory 3.0.0 or newer and Node 22 or newer. (#1165)
