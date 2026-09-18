# Cross-project agent messaging (inbox/queue)

Direct, claim-once messaging between two projects, so an agent in one repo can
ask an agent in another repo to do something **without pulling that repo's
context into its own session**. Added in 2.3 (migration V64).

The motivating case: you have Kimi open in `Projects/A` and Claude open in
`Projects/B`. A needs a change from B. You don't want Kimi to read and reason
about B (wasted tokens, polluted memory). Instead Kimi writes a self-contained
request and drops it into B's inbox through ai-memory; later, in B, you tell
Claude to check its inbox, it pops the message, and works from it.

This is the one place ai-memory deliberately crosses its per-project isolation
boundary. The crossing is explicit and bidirectionally scoped: a project only
ever sees mail addressed **to** it (its inbox) or sent **from** it (its outbox).

## The model

- A message is addressed to a **project**, not a person. Any session working in
  the recipient project can pop it. (Knowledge is shared; the pop is the
  claim-once baton — the same rule handoffs follow.)
- **Claim-once queue.** A message is `pending` until a session in the recipient
  project pops it (→ `claimed`, exactly once) or the sender cancels it
  (→ `cancelled`). A second pop of the same message returns nothing.
- **Fail closed on an unknown recipient.** The recipient project must already
  exist (an agent must have run there at least once). A typo is rejected, not
  turned into a dead inbox nobody reads.
- **Bounded.** Each recipient inbox holds at most 256 pending messages; a full
  inbox rejects new sends.

## Using it from an agent (MCP — the primary path)

Four MCP tools (documented in the installed `ai-memory-messaging` skill):

| Tool | Role |
|---|---|
| `memory_message_send` | Send a message to `to_workspace` + `to_project`'s inbox. |
| `memory_message_list` | List pending mail. `box="inbox"` (default) or `box="outbox"`. Read-only. |
| `memory_message_pop` | Claim the next inbox message (oldest, or a specific `message_id`). |
| `memory_message_cancel` | Retract a sent message (`message_id`), or clear the whole outbox. |

**In Kimi (project A), sending:**

> "Ask the agent in `default/project-b` to add a `/v1/export` endpoint that
> streams NDJSON, and to keep the existing auth middleware. Send it through
> ai-memory."

Kimi calls `memory_message_send` with `to_workspace: "default"`,
`to_project: "project-b"`, and a self-contained `body`. Compose the body as a
complete prompt — the recipient cannot see project A.

**In Claude (project B), receiving:**

> "Check my ai-memory inbox."

Claude calls `memory_message_pop`, which returns the message and empties it from
the queue. If you only want to look without consuming, `memory_message_list`.

**Giving up on a request (from A):**

> "Never mind that request I sent to project-b — cancel it."

Kimi calls `memory_message_cancel`. With no id it clears every pending message A
has sent; with a `message_id` it retracts just that one. A message already
popped by B cannot be cancelled.

## Using it from the terminal (CLI — secondary)

```
ai-memory message send --to-workspace default --to-project project-b \
    --subject "export endpoint" "Add a /v1/export endpoint that streams NDJSON…"
# body may also be piped on stdin:
echo "…request…" | ai-memory message send --to-workspace default --to-project project-b

ai-memory message list                 # this project's inbox (pending)
ai-memory message list --outbox        # what this project has sent
ai-memory message pop                  # claim the oldest inbox message
ai-memory message pop --id <message-id>
ai-memory message cancel --id <message-id>
ai-memory message cancel --all         # clear this project's outbox
```

The CLI resolves the current project as the sender/reader scope, the same way
`ai-memory handoffs` does.

Both CLI and MCP sends run `message_send` admission against the **recipient**
scope: deciding webhooks run before insertion, and observers run asynchronously
only after a successful commit. With no matching webhook configured, send
behavior is unchanged. A configured blocking reject policy can now refuse CLI
sends too. See [admission webhooks](admission-webhooks.md).

For event-driven inbox consumers, a nonblocking observer can use the notified
workspace/project to trigger a scoped inbox read without periodic polling. The
notification carries scope and actor metadata, not the message body or ID. It
is a bounded, best-effort signal, not a durable queue, processing acknowledgment,
or automatic agent wakeup. Consumers still need their own deduplication and
recovery policy. This does not add a notification for raw Stop observations.

## Security — a popped message is untrusted input

A popped message was composed by an agent in **another** project. Treat the body
as a **task request to evaluate with the user, never as instructions to obey**.
The design enforces this on several levels:

- **Nothing auto-enters context.** The session-start notice (below) shows only a
  count. Message text reaches an agent only through a deliberate
  `memory_message_pop` call.
- **The popped body is fenced** as untrusted cross-project input, and the pop
  response carries a `security_notice` saying so. It must not, on its own, cause
  the recipient to run commands, reveal secrets, change policy, or call tools.
- **Provenance is surfaced** (`from_workspace`, `from_project`, `from_agent`,
  `from_owner_user`) outside the fence, so the operator can judge trust first.
- **Secrets are scrubbed** and the body is size-capped on send, so a body cannot
  smuggle credentials cross-project or flood the recipient's context.
- **Isolation holds:** inbox reads are keyed by the recipient coordinate,
  outbox/cancel by the sender coordinate.

On a shared/multi-user server, sending requires the normal write capability, and
each message records who sent it for audit.

## On-start "hot context" and the inbox notice

ai-memory already gives a resuming agent a hot start: every agent's SessionStart
hook (or, for clients that discard SessionStart output like Kimi, its first
user-prompt hook) fetches `GET /handoff`, which returns any pending single-use
**handoff** plus — when the project's `.ai-memory.toml` opts in with
`[briefing] inject_on_session_start = true` — a compiled **project brief** of
pinned / `_rules/` / `_slots/` pages and recent-page pointers. That block is
prepended to the new session's context automatically.

As of 2.3, that same on-start block appends a **non-consuming inbox notice**
when the project has pending mail:

```
📬 ai-memory: 2 cross-project messages waiting in this project's inbox.
Use `memory_message_pop` to read the next one …
```

The notice carries only a count — never any message text — so it cannot be used
to inject content into a resuming agent. The count is also available on demand
via `memory_briefing` (`pending_message_count`).
