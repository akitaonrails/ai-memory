# `[auto_scope]` isolation modes

`ai-memory serve` publishes a process-shared "currently active project"
pointer that MCP read tools consult when the caller omits `workspace` /
`project`. The pointer is fed by foreground lifecycle hooks: session start,
user prompt, and pre-tool events that resolve a `cwd` to a real project update
the pointer so read tools answer for the project the agent is actually in, not
the server's static `--project` default. Completion and shutdown events still
land in their resolved project, but do not advance shared fallback slots: a
delayed post-tool, stop, or session-end tail from an older process must not
redirect a newer session's unscoped reads.

Since v1.39 that pointer is **keyed by the caller's own coordinate** by
default (`per_actor`), so two harnesses in one project — or two operators on
one server — cannot overwrite each other's notion of "current project".

Before v1.39 the default was a single process-wide slot (`single`). That is
right for exactly one harness at a time and collapses as soon as there are
two: a hook firing from `~/repo-A` overwrites the slot that a concurrent
`memory_query` (with no explicit project) in `~/repo-B` was about to read.
Because unscoped **writes** resolve through the same pointer, that could also
land a `memory_write_page` in the wrong project.

The `[auto_scope]` config block still selects the mode explicitly, including
`single` for anyone who wants the historical slot back.

## Modes

| `mode`        | Key                    | When to use                                                                                              |
|---------------|------------------------|----------------------------------------------------------------------------------------------------------|
| `single`      | (none — global slot)   | The pre-v1.39 behaviour. One operator, one harness at a time; last write wins. Opt in only if you want it. |
| `per_session` | `session_id`           | Session-aware clients/bridges that forward the hook session id on every MCP request. |
| `per_actor`   | `(qualified identity, session_id)`, with an identity-only no-session slot | **Default.** Isolates parallel harnesses and separate operators. Falls back to the shared slot for a caller with no coordinate, and for an install where nothing has ever been keyed (no lifecycle hooks); fails closed on a genuine session mismatch. |

Both opt-in modes still publish foreground activity to the single slot in
parallel, so a caller with no actor identity (anonymous probe, legacy code
path) sees the most recently active project rather than an empty pointer.
Non-foreground events refresh only their exact keyed entry when one exists.
That preserves legacy behavior without letting a delayed tail take over, but
it is not per-session isolation; use explicit `workspace` + `project`
arguments when a client cannot send actor identity and concurrent runs matter.

Explicit scope arguments fail closed. A `project` argument is resolved
inside the active workspace first, then inside the server's default
workspace; if neither contains that project, the tool returns an error
instead of falling back to the active/default project. A `workspace`
argument must be paired with `project`, and read/admin maintenance
paths use find-only lookups so typos do not create empty scopes.

## Implementation contract

Scope resolution is centralized in `ai_memory_store::ScopeResolver` and its
explicit helpers:

- `lookup_existing_scope` for read, search, maintenance, retention, embed, and
  destructive paths. It never creates workspaces or projects.
- `create_explicit_scope` for explicit write/create paths only.
- `resolve_many_existing_scopes` for multi-project search scopes, with
  deduplication and max-scope validation.
- `ScopeResolver::resolve_read_args` and `resolve_write_args` for MCP tools
  that also need actor-scoped active-project fallback.

New MCP, admin, or web API routes should use those helpers instead of
hand-rolling `find_workspace` / `find_project` chains. PRs that touch scope
resolution should include table-driven tests for partial scope rejection,
missing explicit scope, active-project precedence, and cross-workspace
isolation.

## Configuration

```toml
[auto_scope]
mode = "single"           # "single" (default) | "per_session" | "per_actor"
session_ttl_secs = 3600   # TTL for per-key entries (default 1 h)
max_entries = 4096        # hard cap; oldest insertions evicted first
```

Environment-variable overrides follow the standard
`AI_MEMORY_<SECTION>__<KEY>` shape:

```bash
AI_MEMORY_AUTO_SCOPE__MODE=per_actor
AI_MEMORY_AUTO_SCOPE__SESSION_TTL_SECS=7200
AI_MEMORY_AUTO_SCOPE__MAX_ENTRIES=8192
```

## Where the actor identity comes from

| Source                                             | Populates                  |
|----------------------------------------------------|----------------------------|
| Hook payload (`/hook?event=…&agent=…`)             | `session_id`, `agent`      |
| Auth middleware (rung 1 root with `root_username`) | `user` ← root_username     |
| Auth middleware (rung 1b trusted proxy)           | username or OIDC `(issuer, subject)` pair |
| Auth middleware (rung 2 DB user)                   | `user` ← `users.username`  |
| MCP request header `X-Memory-Actor-Session-Id`     | `session_id` for tool calls |
| MCP request header `Mcp-Session-Id`                | fallback `session_id` for tool calls |
| Anonymous / no token                               | empty actor → single slot  |

`X-Memory-Actor-Session-Id` means the agent-run session id from the
lifecycle-hook payload. It is not an OIDC/Keycloak login session: the
provider's JWT `sid` claim identifies an IdP browser/device session and
must not be used as ai-memory's actor session key.

`per_session` reads from `session_id`; `per_actor` reads from both the
qualified identity and `session_id`. In `per_actor`, a request that has identity but
no session id can use that user's latest no-session slot instead of the
process-wide single slot. A request that does carry a session id must
match a hook-published keyed entry; if it does not, ai-memory falls back
to the server's baked default rather than another session's latest
project.

The composite `(identity, session_id)` key namespaces only these active-project
pointers. The durable `SessionId` stored for hook observations remains global:
if another owner reuses an already-owned id, ai-memory drops that hook before it
can append observations or publish a pointer for the foreign actor.

Owner and agent are what identify a session; scope is not. The same operator's
session legitimately produces events in another project when its cwd moves, so
a differing `(workspace, project)` is recorded rather than rejected — see
[`[routing] mid_session`](marker-file.md#mid-session-navigation-routing-mid_session)
for how those events are attributed. The one exception is a terminal event: a
`SessionEnd` naming a different scope than its session is not that session's
end, so it is dropped rather than ending someone else's session.

## Client requirements

Lifecycle hooks already include the agent-run session id in their
payloads. MCP tool calls are separate HTTP requests, and most built-in
MCP client config files can only declare static URL/auth headers. Static
configs cannot inject the current agent-run session id into every tool
call.

Claude Code can opt into ai-memory's session-aware stdio bridge:

```bash
ai-memory install-mcp --client claude-code --session-aware --apply
```

The bridge reads the `CLAUDE_CODE_SESSION_ID` that Claude supplies to its stdio
MCP subprocess and forwards it as `X-Memory-Actor-Session-Id` while preserving
the configured remote endpoint and bearer token. Existing Claude Code installs
stay on the static HTTP transport unless this flag is used.

Claude Code's stdio MCP subprocess keeps the id it received at startup across
`/clear`, even though subsequent hooks receive the new id. On
`--continue`/`--resume` without an explicit id, Claude may also give the MCP
subprocess the startup id rather than the resumed id. Restart Claude Code after
`/clear` when exact session-key continuity matters, and prefer
`--resume <session-id>` for explicit resumes. The bridge deliberately fails
when no `CLAUDE_CODE_SESSION_ID` is available instead of silently degrading to
the shared single slot.

Use `per_session` only when your client or bridge can send the same
opaque session id from the hook payload on each MCP request as
`X-Memory-Actor-Session-Id` (preferred) or `Mcp-Session-Id`. Otherwise
requests that carry a different MCP session id fail closed to the baked
default, while requests with no usable actor identity still degrade to
the legacy single slot.

OIDC/Keycloak authentication can identify the human user, client, and
agent, but it does not automatically identify the current coding-agent
session. If a gateway validates a Keycloak JWT, it should propagate
`X-Memory-Actor-User` or the `Issuer` + `Sub` pair, plus optional `Client` /
`Agent`; it should only emit
`X-Memory-Actor-Session-Id` when a real agent session id has been
forwarded by a session-aware bridge.

For built-in installs that use static MCP config, prefer:

- `single` for one operator / one active project at a time.
- `per_actor` with multi-user bearer auth when several humans share one
  server. It isolates users via authenticated actor keys; same-user
  concurrent sessions still need explicit `workspace` + `project` args
  or a session-aware bridge when the MCP client cannot forward the hook
  session id.
- `per_session` plus Claude Code's `install-mcp --session-aware` bridge when
  one operator runs concurrent Claude Code sessions in different projects.

## Pairing with multi-user mode

`per_actor` is most useful when the engine is in multi-user mode (see
[`docs/users.md`](users.md)) — each native API credential resolves through
`api_credentials` to its owning `users` row, so the auth middleware tags
every request with the right `user`. With `[auto_scope] mode = "per_actor"`, two
authenticated users running concurrent agent sessions through the same
engine no longer overwrite each other's "current project" pointer for
MCP calls; if their clients also forward session ids, concurrent
sessions by the same user are isolated too.

Single-user installs can use `per_session` alone (no `token_pepper`,
no `users` row) only when the client/bridge forwards the session id on
MCP calls. Claude Code has the opt-in bridge above; with other stock static MCP
configs, use explicit `workspace` + `project` arguments for concurrent windows.

## Memory footprint

Per-key entries are tiny: two `Uuid`-sized ids + an `Instant`. With
the default `max_entries = 4096`, the map worst-cases at ~tens of KB
even on a corporate engine fielding hundreds of concurrent sessions.
The TTL ensures stale entries (closed Claude Code windows, dropped
hook clients) age out within an hour; the cap drops the oldest
insertions first if the TTL window is somehow exceeded.


## Upgrading to v1.39

The default changed from `single` to `per_actor`. For most installs nothing
observable changes:

| your setup | before | after |
|---|---|---|
| one harness, hooks installed | shared slot | keyed slot for that session — same project |
| static MCP client (bearer, no session id) | shared slot | that operator's identity-only slot — same project |
| client that forwards no identity at all | shared slot | shared slot, unchanged |
| **MCP-only install, no lifecycle hooks** | shared slot | shared slot — nothing is ever keyed, so the fallback applies |
| two harnesses / two operators | one slot, last write wins | one slot each |

The single behavioural change is deliberate: a client that forwards a session
id which does **not** match any published hook activity no longer inherits the
shared slot. Answering it from there is what routed a request into whichever
project published last — on a shared server, potentially another operator's.
Such a caller now resolves to the server's configured default instead.

To restore the old behaviour exactly:

```toml
[auto_scope]
mode = "single"
```

or `AI_MEMORY_AUTO_SCOPE__MODE=single`. The effective mode is logged at
startup (`active-project isolation mode mode=…`), which is the quickest way
to confirm what a running server is using.
