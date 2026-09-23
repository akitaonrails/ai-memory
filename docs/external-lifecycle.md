# External lifecycle capture

An orchestrator that observes its agents can send lifecycle events to one shared
ai-memory server through `/hook/batch`. Framework adapters belong outside core;
the server continues to own storage, retrieval, consolidation and durable memory.

## Choose one capture path before launching

Set `AI_MEMORY_CAPTURE_OWNER` to your producer namespace in the environment of
the harness you launch, for example `example.runtime`. Any non-whitespace value
opts that process and its inheriting children into external capture. Unset,
empty and whitespace-only values preserve normal native capture.

```text
Orchestrator
  |-- launches harness with AI_MEMORY_CAPTURE_OWNER=example.runtime
  |-- records its lifecycle events with stable IDs
  `-- sends those events through POST /hook/batch

Harness
  |-- installed hooks keep delivering supported startup context
  `-- MCP keeps providing recall and deliberate memory writes
```

Updated native hook commands, POSIX/PowerShell hook bundles, and generated
OpenCode/OpenCode 2/OMP/Pi/OpenClaw integrations honor the variable. Reapply
`install-hooks` after upgrading to refresh staged scripts or generated code.
Older installations do not understand this context.

| Responsibility | With external capture |
|---|---|
| Native lifecycle observations | Skipped before spool, queue or POST |
| Native CLI hook drains and SessionStart backfill | Skipped |
| Supported handoff, briefing and inbox-notice delivery | Preserved |
| Native identity used by handoff/session-aware MCP | Preserved |
| MCP recall and deliberate writes | Unchanged |
| Existing repository capture policy | Unchanged; an opted-out repository stays opted out |

Claude Code receives context through SessionStart. Kimi Code receives it through
UserPromptSubmit because it discards SessionStart output. The same existing
delivery paths remain in use. Context delivery may register the receiving session
and claim a handoff; suppressing capture does not make it read-only. Harnesses
without automatic injection can still use `memory_handoff_accept` through MCP.
Do not also claim that handoff in the orchestrator if the hook owns delivery.

This is inherited process context, not server configuration, a security identity,
or a lease. Keep it out of the shared server's environment and global shell
configuration. Standalone launches without it continue to capture normally.
A runtime hosting multiple agents in one process applies the variable to all its
installed ai-memory integrations; it must manage finer capture ownership itself.

`ai-memory hook --check-capture` reports `external_capture: true` and
`admits_capture: false` when this context is active. It performs no ingestion or
handoff delivery. The producer namespace itself is not printed or authenticated.

## Reuse the public event contract

For a live harness, send its actual wire `agent` (such as `claude-code` or `codex`)
and native `session_id`. Use the same session ID, cwd, scope and authenticated
operator as its remaining native integration. The body's session ID takes
precedence over a query parameter. Replacing only the ingestion ID with an
orchestrator's logical agent ID separates it from native handoff/MCP identity.

Use explicit `workspace` and `project` for each event. Preserve canonical
lifecycle names (`session-start`, `user-prompt-submit`, `post-tool-use`,
`session-end`, etc.) and the supported harness payload shape. `session-end`
means the execution ended, not a turn completed. Sessions containing only
unknown events are not substantive sessions for automatic summaries.

Set `extension=<producer-namespace>` and an explicit `source_event=<event-name>`
for provenance. Both are needed to preserve the namespace on recognized events.
`extension` accepts up to 64 ASCII token characters; `source_event` accepts 128.
Letters, digits, `.`, `_`, `-` and `:` are allowed. These fields describe the
source; they do not expand the core event enum or retain arbitrary metadata.

Keep logical agents, parent/child relationships and workflow IDs in adapter-side
state for now. Do not repurpose native `agent_id` fields: some harness payloads
use them as subagent markers. The existing managed-workstream `managed_run`
field also has its own lease, ledger and handoff behavior.

### Stable retry identity

`ingest_key` is an existing project-scoped key, independent of `extension`.
Construct it once from stable event identity and persist it with the event before
delivery. Never hash prompt/tool content to decide whether events are equal.

For example, an adapter using Node can derive a 64-character key as follows:

```javascript
import { createHash } from "node:crypto";

function ingestKey({ producer, actor, agent, sessionId, sourceEvent, eventId }) {
  const identity = JSON.stringify([
    "external-capture-v1", producer, actor, agent,
    sessionId, sourceEvent, eventId,
  ]);
  return createHash("sha256").update(identity, "utf8").digest("hex");
}
```

Here `actor` is a stable adapter-side operator namespace, never a bearer token.
Include the coordinates within which `eventId` is unique. An adapter with
producer-wide unique event IDs may use a smaller tuple. Keep its encoding and
values stable across process restarts and implementation languages.

The same producer/event identity yields the same key on retry. Distinct event
IDs or producer namespaces yield distinct keys even for equal payloads. Reusing
the raw event ID as `ingest_key` under two extensions does **not** namespace it.
Keys accept 1 to 64 ASCII letters, digits, `_` or `-`. Malformed keys fall back
to unkeyed ingestion, so validate them before sending.

The server claims the key and observation in one transaction. A completed replay
is skipped; a pending replay resumes downstream work without inserting another
observation. Keys expire after **30 days**. This is bounded retry protection, not
permanent deduplication or an exactly-once guarantee for every wiki side effect.

### Ordered batches and recovery

A batch uses the existing array of `{url, body}` items:

```json
[
  {
    "url": "/hook?event=user-prompt-submit&agent=claude-code&workspace=team&project=app&extension=example.runtime&source_event=user-prompt-submit&ingest_key=example-runtime-run-17-prompt-4",
    "body": {
      "session_id": "native-session-17",
      "cwd": "/work/app",
      "prompt": "Continue the parser fix."
    }
  }
]
```

Send this JSON to `POST /hook/batch` with the normal bearer authentication. The
URL in each item supplies the event query; the server does not fetch that URL.
The short key above assumes the runtime assigned a producer-wide unique run ID.
Use the tuple recipe when event IDs have narrower scope.

- At most 256 items fit in a batch. Existing body limits and sanitization apply.
- Batch processing happens inline, in array order. There is no global ordering
  across concurrent requests and no all-or-nothing transaction for a batch.
- Prefer `accepted_indices` when present. Otherwise `accepted` is the contiguous
  acknowledged prefix. Preserve every unacknowledged item for retry.
- A rate-limited source can be skipped while other sources advance. Inspect
  acknowledgements even on HTTP 429 or a partial failure; `failed_index` identifies
  a processing failure after earlier skips. An acknowledgement can also mean a
  deliberate policy drop, not a new observation.
- On a timeout or lost response, retry unchanged items and keys. Back off on
  saturation rather than opening an unbounded number of requests.

Maintain a durable producer-side queue if offline/restart recovery matters.
Flush earlier events for a session before sending its terminal event. Serialize
delivery within a session where order matters; different sessions can share the
server concurrently. Late observations and a later terminal event use existing
resumed-session behavior. Stopping a parent does not synthesize endings for its
children; the orchestrator must report those lifecycle transitions.

`POST /hook` remains available, but its `202` response precedes persistence.
Prefer `/hook/batch` when the producer needs an inline processing acknowledgement.

## Boundaries

The variable does not erase or cancel existing spool entries. Explicit
`hook-drain` and other standalone hooks can still deliver that backlog.
Generated integrations also retain their existing queue disposal behavior,
which can drain previously captured events. Choose the capture owner before
the execution's first event; switching midway cannot deduplicate events already
produced by independent paths.

Manual backfill, the importer, `finalize-session` and managed transcript import
through `ai-memory run` retain their existing behavior. Do not combine them as
independent lifecycle producers for the same execution. External capture does
not disable native harness transcript files or other vendors' hooks.

Auth, owner checks, admission rules and sanitization still apply. All items in
one batch share one authenticated identity; separate batches by operator when
representing multiple users. `extension` is self-declared provenance and the
capture variable grants no permissions. ai-memory has a shared single-tenant
wiki with multi-user attribution, not per-project RBAC or producer isolation.
See [multi-user attribution](users.md).

No framework adapter, workflow engine, generic plugin runtime or OpenTelemetry
collector is introduced. A future OpenTelemetry adapter can map conversation
and tool-call identities at the boundary. Keep ingestion producer identity
separate from the model provider and preserve the native session identity needed
by ai-memory's existing integrations.
