# ai-memory-relay

A companion CLI for sending externally captured lifecycle events to ai-memory.
It records events in a local SQLite queue and sends them through `POST /hook/batch`.
Use it when an orchestrator needs delivery to survive an unavailable server or a
process restart.

The relay has its own Cargo workspace. It does not open ai-memory's database or
wiki, launch agents, or claim handoffs. See the
[external lifecycle contract](../../docs/external-lifecycle.md) for capture
ownership and the [companion policy](../../docs/companion-crates.md) for the core
boundary.

## Build and use

From the repository root:

```sh
cargo build --locked --manifest-path companions/ai-memory-relay/Cargo.toml
```

The executable is `companions/ai-memory-relay/target/debug/ai-memory-relay`, unless
`CARGO_TARGET_DIR` selects another target directory. The commands below assume
the executable is on `PATH` and an ai-memory server is running.

Initialize a queue. Its parent directory must exist; the relay creates the queue
directory itself.

```sh
ai-memory-relay init \
  --queue-dir "$HOME/.ai-memory-relay" \
  --server-url http://127.0.0.1:49374 \
  --producer my-orchestrator \
  --actor developer \
  --workspace work \
  --project example
```

This binds the queue to one destination and scope. Repeating an identical `init`
is safe. A different binding is rejected. `actor` is a stable namespace used to
derive retry keys; authentication still comes from the server's bearer token.
It does not select or impersonate a server user.

When the orchestrator launches a harness whose lifecycle it captures, pass
`AI_MEMORY_CAPTURE_OWNER=my-orchestrator` to that process. Native capture is then
suppressed as described in the lifecycle contract. Supported handoff delivery
remains active, and the agent can keep using MCP for retrieval and deliberate
memory writes. A standalone harness without that context keeps normal hooks.

The orchestrator supplies a JSON array, for example `events.json`:

```json
[
  {
    "event_id": "event-001",
    "agent": "claude-code",
    "event": "session-start",
    "body": {
      "session_id": "native-session-123",
      "cwd": "/workspace/example"
    }
  },
  {
    "event_id": "event-002",
    "agent": "claude-code",
    "event": "user-prompt-submit",
    "body": {
      "session_id": "native-session-123",
      "cwd": "/workspace/example",
      "prompt": "Check the failing test."
    }
  }
]
```

```sh
ai-memory-relay enqueue --queue-dir "$HOME/.ai-memory-relay" --file events.json
ai-memory-relay flush --queue-dir "$HOME/.ai-memory-relay"
ai-memory-relay status --queue-dir "$HOME/.ai-memory-relay"
```

`enqueue` validates the whole array and commits it in one transaction. Each body
must be an object with explicit `session_id` and `cwd` strings. Use the harness's
native identity and canonical hook event names. Other body fields keep their
values, including an `_ai_memory_capture` block. The relay does not translate
provider transcripts or infer parent agents and workflows.

For authenticated servers, supply `AI_MEMORY_AUTH_TOKEN` through the flush
process's environment. The relay does not store or print it. The destination
must use HTTP or HTTPS, with no credentials, query string or fragment in the
base URL. Redirects are refused.

## Delivery and retries

Each event gets a deterministic `ingest_key` from the producer, actor, native
agent/session identity, event name and producer-assigned `event_id`, following
the lifecycle contract. `extension` carries the producer and `source_event`
carries the event name.

Re-enqueueing the same identity and body is recognized while the pending event
or receipt remains in the queue. JSON object key order does not matter. Reusing
that identity with different content rejects the entire input array. Two equal
bodies with different event IDs remain separate events.

Only the oldest pending event from each session enters a batch. The next event
for that session is eligible after acknowledgement. This keeps `session-end`
behind earlier events while allowing other sessions to proceed. Order is the
queue's committed enqueue order; producers must serialize events within a
session before enqueueing them. Separate queue directories do not coordinate
session order.

One process can flush a queue at a time. Other processes can enqueue during a
flush. HTTP runs outside the SQLite write transaction. If a response is lost
after the server commits, the event stays pending and a retry uses the same key.

The relay validates the complete batch acknowledgement before changing its
queue. It honors both contiguous and noncontiguous accepted indexes, including
partial success in HTTP 200 or 429 responses. Malformed acknowledgements,
authentication failures and transport errors retain unacknowledged events.
A failed head blocks its own session. HTTP 429 ends the flush; the caller should
back off before scheduling another one.

An acknowledgement can also mean the server deliberately dropped an event, for
example because of capture policy or a session identity collision. A drained
queue does not prove that every event became an observation. The relay catches
a native session changing agents within one queue, but server ownership checks
still apply across queues and producers.

The server's ingest keys expire after 30 days. The relay records the first
attempt before sending and retains expired pending events without replaying
them automatically. Unsent events can remain offline longer. Keep the host
clock accurate; the retry deadline uses wall-clock timestamps. Recreating a
queue discards its attempt history and receipts, so it is not a safe way to
recover an ambiguous delivery after the retry window.

`flush` attempts at most 64 batches by default; `--max-batches` changes that
count. Requests, transport retries and the total flush duration also have finite
bounds. Unfinished work stays queued for the next invocation.

Exit codes are:

| Code | Meaning |
| --- | --- |
| `0` | Command succeeded; a flush has no pending events. |
| `2` | Command failed. Inspect stderr; unacknowledged events remain queued. |
| `3` | Flush ended with events still pending. |

`status` emits a JSON object with counters, including `pending_items`,
`pending_bytes`, `pending_sessions`, `expired_items` and `receipts`. It does not
print event bodies. Receipts count acknowledgements, including policy drops.

## Local data and recovery

The queue contains producer-supplied event bodies before server sanitization.
The producer must apply its capture exclusions before enqueueing. The relay
does not load the project's `[capture] ignore_paths` settings or inspect tool
payloads for sensitive paths. Server sanitization cannot protect a local queue
that already contains those payloads.

On Unix, new queue directories use mode `0700` and files use `0600`. Existing
queue paths must already be private. Queue symlinks and Windows reparse points
are rejected. On Windows, the relay does not set ACLs; use a directory restricted
to the account that runs the orchestrator.

SQLite uses WAL mode and `synchronous=FULL`. Keep the database and its sidecar
files together. To move or back up a queue, stop producers and flush processes
first. Reopen the same queue after a restart; do not edit its SQLite tables to
mark events as delivered. An unknown queue schema is rejected.

The queue uses these limits:

| Resource | Limit |
| --- | --- |
| Pending events | 50,000 |
| Pending body bytes | 64 MiB |
| Pending events plus retained receipts | 200,000 |
| Retained session identities | 50,000 |
| One event body | 256 KiB |
| One input file | 32 MiB and 10,000 events |
| One HTTP batch | 8 MiB and 256 events |
| Acknowledgement body | 64 KiB |

Reaching a limit rejects new input instead of dropping older events. Receipts
and unused session records can expire after the 30-day retry window. Pending
events are retained. Review expired entries and producer failures before
choosing how to archive a queue; the CLI has no command that discards them
automatically. These are logical record limits, not a fixed SQLite file size.

## Validation

The package needs separate checks because it is outside the root workspace:

```sh
cargo fmt --check --manifest-path companions/ai-memory-relay/Cargo.toml
cargo clippy --locked --manifest-path companions/ai-memory-relay/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path companions/ai-memory-relay/Cargo.toml
```

Run the integration test against locally built binaries:

```sh
cargo build --workspace
cargo build --locked --manifest-path companions/ai-memory-relay/Cargo.toml
uv run --no-project python tests/e2e/external_relay_smoke.py \
  --ai-memory-bin "$PWD/target/debug/ai-memory" \
  --relay-bin "$PWD/companions/ai-memory-relay/target/debug/ai-memory-relay"
```

Adjust the binary paths if `CARGO_TARGET_DIR` is set; Windows executables end in
`.exe`. The test starts an isolated real ai-memory server and invokes its native
hooks. It checks persistent observations for offline recovery, retries after a
lost response, distinct event identities and 15 concurrent sessions. It also
checks handoff delivery with native capture suppressed, rejected authentication
and acknowledged session collisions. Temporary data and logs are retained, and
the test prints their directory.
