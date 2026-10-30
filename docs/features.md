# Features and activation

This guide covers prerequisites, activation and verification for shipped
features. Follow the linked guides for the full options.
Configuration examples belong in `<data_dir>/config.toml` unless a section names
`.ai-memory.toml`. Restart the server after changing server configuration.

## Start here

The web version of this guide includes "Effective installation state and next
steps", evaluated for the current caller. The authenticated
`GET /api/v1/docs/features/status` endpoint exposes the same safe facts. Settings
come from the server's startup snapshot, while permissions are checked on every
request. Restart after changing server configuration. A configured provider can
still be unhealthy, so use Health to diagnose it. The server cannot verify
remote hooks, client versions, marker files or browser preferences. Missing host
wiring reads "Not reported", and administrator settings read "Restricted"
without the required capability. The Markdown version is a reference that
matches the build, not a live configuration dump.

The built-in dashboard and this documentation need the web surface enabled:

```bash
ai-memory serve --transport http --enable-web
```

Open `/web/docs`, or the corresponding path under your configured base path and
web slug. Documentation is embedded in the binary, including Docker images; no
checkout, internet connection, LLM, or database migration is needed to read it.
It shares the web server's authentication. It describes capabilities, not which
optional features an individual running server has enabled.

Install the native client on each agent machine and configure the server URL
and credentials following the [installation guide](install.md). Updating a
server container does not update clients or hooks on a separate machine.

## Capture and agent integration

Lifecycle hooks capture bounded, sanitized observations from an agent. They
require a reachable server and a supported harness installed locally.

The managed launcher can wire supported integrations on first launch:

```bash
ai-memory run codex
```

To wire a client you launch directly, preview and then apply its integration:

```bash
ai-memory install-mcp --client codex
ai-memory install-mcp --client codex --apply
ai-memory install-hooks --agent codex
ai-memory install-hooks --agent codex --apply
```

Use `claude-code` for Claude Code, or the exact supported name from the
[installation guide](install.md#configuring-other-agent-clis). MCP provides tools;
hooks provide capture. Installing only one does not configure the other.

Verify: start a session and perform work. Open its project in the dashboard;
registered sessions and captured-event counts should grow. If they do not, use
[diagnostics](diagnostics.md) and check the client executable, URL and credentials.

## Token accounting and investigation

The dashboard shows usage for individual model responses and totals per session
and agent type. Update the native Claude Code or Codex client and reinstall its
hooks as above to enable capture. There is no separate dashboard token switch.
The server records reports as they arrive; the session need not end.

The Token usage form filters the complete authorized response ledger on the
server before selecting a 50-row page. From is inclusive and Until is exclusive,
both in UTC, and a date-only URL means midnight UTC. Model and tool names match
exactly. Harness, session and native turn filters intersect the current project,
session and ownership boundaries. Pagination and reload keep every filter. The
matching-token total covers the filtered ledger, while the summary cards keep
the whole authorized scope for comparison. Response time is used when reported,
and older rows fall back to capture time.

Open "Turn & tools" on a response to see the native response ID, turn,
timestamp basis, session and bounded tool references. Its links narrow the
ledger to that turn or tool. Claude tool requests and results use explicit IDs.
Codex tool names are marked as same-turn associations. An association does not
split token cost across tools or show causation. Arguments, outputs and prompts
are excluded. Update the native binary on each agent machine to capture this
metadata, because updating the server container alone cannot update remote
clients. Existing rows stay unknown unless a native client replays their
original transcript. The extractor follows the
[Claude tool-call schema](https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls)
and [Codex session usage records](https://github.com/openai/codex/blob/main/codex-rs/core/src/state/session.rs).

Verify: open the project, then the agent, then the session, and inspect "Token
usage". Project and session usage panels refresh every ten seconds. Search,
signal filters and ordering cover all captured responses, and page controls show
fifty at a time. Input, output and cache are separate, and repeated reports for
one response are deduplicated. Agent cards show coverage.

Missing reports mean unknown usage. Unsupported harnesses, older hooks, and
deleted or unsupported transcript records can leave gaps. Updated hooks resume
readable history in bounded batches on later events, so a large backlog may need
several hooks before all responses appear. Large-context, long-output and
above-average signals are investigation hints, not confirmed failures or
monetary costs. See [dashboard usage](usage.md#investigate-token-usage-in-the-dashboard).

## Themes and responsive navigation

Choose "Light", "Dark" or "System" in the header. The browser saves your
preference locally. No server setting or restart is required.
On small screens, open the menu for projects, inbox, search and documentation.

Inside a project, "Overview", "Agents", "Token usage", "Sessions" and
"Knowledge" are separate tabs: only the selected section is shown. Overview
contains the summary and pending messages; the other tabs hold their respective
cards, response diagnostics, session list and compiled pages. The URL preserves
the selected tab for sharing, reloads and browser back/forward. Use Left/Right,
Home or End while focused on the tabs to navigate by keyboard. "Inbox" and
"Link graph" open their own pages. The tabs wrap on mobile without hiding
destinations off-screen.

## Agent inbox and directed messages

Queue a request for a project, optionally targeting a harness such as Codex
or Claude Code. The recipient must have a captured session in that project to
appear in the browser selector.

Open "Inbox" to see queued counts by workspace and project, with a direct link
to each mailbox. With no project in the URL, a mailbox with waiting messages is
selected first. The heading names the open mailbox, and opening it does not
collect messages. The sidebar checks for pending messages right away and every
ten seconds while the page is visible. A count and up to five project links
appear only when mail is waiting for an agent, and they disappear after
collection. Each link opens "Awaiting collection", so older pending mail is not
buried behind collected history. Use "Message state" to switch back to all
messages or another state.

To compose, choose "From project". "To project" defaults to the same
project; change it explicitly to send across projects. "To agent" lists only
harnesses registered in that destination, and changing either project clears
the chosen agent. Projects without captured sessions show an empty state and
disable sending. Write the request and select "Queue message". Sending
requires write access to both projects. A human login uses its browser session
and CSRF protection. The older GET-only
login requires explicitly entering a write token for each send; the composer
clears it after the request and does not persist it.

Verify: inspect "Sent". "Queued" means pending; "Collected" means an
agent claimed the message, not that it completed the work. Reading the web
inbox never claims mail or launches an agent.

To change a message that is still queued, open it in "Received" and select
"Edit". The message leaves the queue while you edit, so no agent can collect it.
"Save and requeue" puts the new text back in its original place, and "Discard
changes" puts the old text back. Held messages show as "Editing" and are counted
above the list. A message an agent already collected cannot be edited.

A static MCP client explicitly names the project and receiving harness:

```json
{"workspace":"team","project":"app","agent":"codex"}
```

Pass those arguments to `memory_message_pop`, using your actual registered
scope names. Session-aware clients can use their recorded identity. A terminal
client can receive the same request:

```bash
ai-memory message pop --workspace team --project app --agent codex
```

Messages are claimed once. Read the [messaging guide](agent-messaging.md) for
shared queues, cancellation, sender provenance and untrusted-message handling.

## Handoffs and compaction recovery

Install lifecycle hooks to carry useful context into the next session or
harness. Handoff capture and recovery follow the
configured lifecycle policy; no LLM is required for the deterministic path.
Use `ai-memory run` for managed cross-harness continuity.

Verify: inspect the next session's startup context and handoff state.
An interrupted session may remain open without a session-end event; that alone
is not evidence of a running process. See [handoffs and recovery](usage.md).

## Managed sessions and workstreams

Launch a supported harness with `ai-memory run <harness>` to enable managed
sessions for that launch. Direct launches remain unmanaged; this setting does
not change the global mode.
Use the documented resume and launch-profile options for account-specific
configuration. Root can inspect recorded lifecycle metadata in "Workstreams".

Verify: open the workstream and its linked sessions. The dashboard reports
recorded state; it does not infer live process state from prompt text.
See [managed workstreams](managed-workstreams.md).

## Recall, project routing and memory pages

Each consulted page includes "Knowledge evidence and validity" for its indexed
version. Readable source sessions link to their timelines, and an observation
citation identifies the observation within its source. Source scope and session
ownership are checked separately from shared page access. Opaque feedback and
rewrite IDs get no invented source links. A missing citation leaves the evidence unknown; it does not establish that
the page is unsupported. Expired pages stay available through an explicit URL but leave
normal retrieval. Declared contradictions include only live, readable far ends,
and they do not decide which statement is correct. Evidence pages hold twenty
citations.

Project searches accept canonical and legacy project keys; their result links,
breadcrumb and scope selector use the current stored name. Searching does not
rename the project.

Search result links carry the query, filters, result limit and the project or
global mode. "Recompute explanation" runs the same authorized ranker on demand
and shows stream ranks, RRF contributions, authority and optional reranking
beside the page. It may contact configured embedding or reranking providers. The
result is a new computation, not a snapshot of the original search.
Cross-project search reports its FTS-only stream, and a direct page URL has no
inferred retrieval reason.

Default: lexical retrieval and rule-based summaries work without an LLM.
Connect MCP, then use the installed retrieval skills and `memory_query`.
Project memory is separate from this product documentation.

For explicit project identity, use the registered names in `.ai-memory.toml`:

```toml
workspace = "team"
project = "app"
```

These are illustrative names. Obtain real names from the server or operator;
do not infer them from a directory. Static MCP clients send both scope fields;
session-aware clients can use automatic routing.

Verify: check the resolved scope before writing and confirm the expected
project's pages appear. See [marker files](marker-file.md), [usage](usage.md),
and the [configuration reference](ARCHITECTURE.md#configuration-configtoml).

## Embeddings and reranking

For semantic retrieval and optional candidate reranking, configure the
embedding provider, model and credentials described in
[LLM providers](llm-providers.md) and the [configuration reference](ARCHITECTURE.md#configuration-configtoml).
Availability and defaults depend on the provider configuration and build.
LLM reranking is separately opt-in through `AI_MEMORY_RERANKER=llm` and requires
an appropriate configured LLM provider.

Verify: use query explanations and diagnostics to inspect the active ranking
streams. Keep provider/model/dimension aligned with stored embeddings; stale
vectors are ignored. Ordinary lexical recall remains available without them.

## LLM consolidation and provider fallback

To compile captured evidence into maintained knowledge with an optional model
provider, configure `llm_provider`, the model and the provider's credential flow
from [LLM providers](llm-providers.md). Provider support and
credential types are documented there; do not paste credentials into pages.

Without an LLM, capture, lexical recall and deterministic summaries still work.
Provider calls are separate from the agent harness's own model calls.
Session-end provider consolidation is opt-in; see the provider guide for
`AI_MEMORY_CONSOLIDATE_ON_SESSION_END` and checkpoint behavior.

Verify: inspect compiled pages, jobs and diagnostics after a documented
consolidation trigger. Configure ordered fallback only if needed; the
[provider guide](llm-providers.md) links the fallback reference.

## Learning and proposal review

Prerequisite: a configured LLM provider and eligible completed sessions.
The learning scheduler is enabled by default when a provider exists; cadence
and preflight filters can postpone work. To require human review:

```toml
[auto_improve]
require_approval = true
```

To stop scheduled review while retaining manual operations:

```toml
[auto_improve.scheduler]
enabled = false
```

Verify: inspect "Learning review" as root and the pending-writes audit
trail. Proposals auto-approve by default when approval is not required. Read
the current-behavior section of [auto-improvement](auto-improvement-loop.md)
before applying examples from its historical design sections.

## Capture exclusions and assistant text

Enable exclusions: add a `[capture]` section to the nearest project marker,
using the path patterns documented in [marker files](marker-file.md#capture-exclusions).
Excluded recognized file-tool events are dropped before transport and storage.
Sanitization remains active for accepted content.

Assistant-text capture is a separate opt-in: the server's `capture_assistant`
setting and client hooks installed with `--capture-assistant` must agree.
See [installation](install.md#capture-policy-capability-and-refresh).

Verify: use synthetic, non-secret test content and check that excluded
content does not appear in the scoped timeline. Do not test with real secrets.

## Authentication, team access and the management console

Enable: follow [authentication setup](install.md#human-password-bootstrap-and-recovery)
and [security guidance](security.md). Keep the existing root/user capability
boundary and grant project access explicitly. Serving beyond loopback needs the
appropriate authentication, host and transport configuration.

The management console additionally requires a feature-enabled build and a
runtime flag:

```bash
cargo build --locked --release -p ai-memory-cli --features management-ui
./target/release/ai-memory serve --transport http --enable-management-ui
```

The standard image does not include this optional console. For a source-built
Docker image, use `--build-arg AI_MEMORY_FEATURES=management-ui` and enable the
runtime setting described in [management console](management-ui.md).
Session-management and self-service-key APIs also have independent build/runtime
switches; follow that guide's links for those options.

Verify: root can access operator pages; a regular user sees only permitted
project and owned-session data. Native API keys and human browser sessions have
different capabilities. See [SSO](sso.md) for OIDC device identity.

## External clients and lifecycle capture

Enable: choose the integration path that fits the client. Applications can
use public MCP tools through the [programmatic guide](programmatic-memory.md).
An orchestrator that owns lifecycle events can use the external capture path
and optional relay described in [external lifecycle](external-lifecycle.md).
These companions are installed separately from the core server.

Verify: retries with the same event identity do not duplicate capture,
project coordinates remain correct, and native and external capture do not
both own the same process's lifecycle.

## Backup, retention and deployment

Enable: use the documented deployment and maintenance settings for your
installation. Backups are explicit online operations; restore and destructive
commands have different live-process and confirmation requirements.

```bash
ai-memory backup --to /path/to/private-backups/memory.tar.gz
```

Verify: retain the archive outside disposable container layers and follow
[lifecycle operations](lifecycle-ops.md) before testing restoration. Keep the
server volume, credentials and client installations when upgrading. See
[deployment](deploy.md), [HTTPS](https-via-proxy.md) and [diagnostics](diagnostics.md).

## Documentation and changelog access for agents

The authenticated JSON endpoints are read-only product references:

```text
GET /api/v1/docs?q=token
GET /api/v1/docs/features
GET /api/v1/docs/features/status
GET /api/v1/docs/changelog?release=Unreleased
```

The catalog returns document IDs, titles, categories, descriptions and source
paths. A document response adds Markdown, the build's package version, and
release choices when applicable. It contains no project/session data or live
configuration secrets. Use your configured server prefix and credentials.

For Markdown, use `/web/docs/features.md` or
`/web/docs/changelog.md?release=Unreleased`. Change `/web` to your configured
web slug. The changelog defaults to one release; request `release=all` for its
whole major series, or choose an archived series in the catalog.

In the web changelog, selecting a release loads its notes immediately and updates
the shareable URL, outline and Markdown link. Without JavaScript, use "Show
release" to submit the selection.

[Changelog](../CHANGELOG.md) separates Unreleased work from tagged releases.
Guides and notes are bundled with this binary; references outside the embedded
catalog open their repository source on GitHub and require internet access.
Documentation lookup does not enable a feature or execute its examples.
