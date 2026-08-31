# Admission webhooks: pre-persistence HTTP hooks

> Operator-configured HTTP hooks invoked on the engine's write path
> (`Wiki::write_page`, `delete_page`, `purge_project`, `purge_workspace`,
> `move_project`, and handoff lifecycle operations)
> just before the durable mutation commits. Write hooks can mutate the page
> (return a new frontmatter / body); delete/purge/move hooks are notifications
> that can observe, mirror, or reject. Sourced from
> `crates/ai-memory-wiki/src/admission.rs` and the wiring in
> `crates/ai-memory-cli/src/commands/serve.rs` — keep both as the
> canonical reference if anything here drifts.

## 1. What this is (and isn't)

| | What you can do | What you can't do |
|---|---|---|
| Chain | Add canonical frontmatter fields (e.g. `contributors`); mirror the write into an external system (git, search index, audit log); reject writes that fail policy (e.g. `validate-no-secrets`). | Talk back to the engine's writer or store directly — the chain only sees one page at a time and mutates it in place. |
| Engine | Stays closed for modification: every new behaviour ships as an independent HTTP service in any language. | The engine doesn't discover or auto-register webhooks — operators name them in config. |

If your idea doesn't fit "mutate the page or observe the write", it's
probably a different extension point (`/hook` ingress, `/admin/*` admin
surface, or out-of-band scheduled job).

## 2. Lifecycle

The blocking chain fires inside `Wiki::write_page`, **after** the markdown is
parsed and initially sanitised but **before** the atomic write and store upsert.
Webhook mutations are sanitised again before persistence. A mutation applied
by a webhook propagates to both the on-disk markdown file and the SQLite row
in a single atomic step (see
[ARCHITECTURE.md](ARCHITECTURE.md) on the writer actor / atomic write
invariants).

Webhooks run **sequentially**, in the order declared in config. Each one
sees the (possibly mutated) page from the previous webhook — so a
`contributors` hook that adds frontmatter is followed by a `git-mirror`
hook that mirrors the enriched page.

Webhooks fire on these `op` values today (extensible enum):

- `write_page` — direct writes via MCP `memory_write_page`, the CLI
  `write-page`, `/admin/write-page`, the lint rewriter, hook synthesis.
- `consolidate` — LLM consolidation writes from the consolidator
  (SessionEnd opt-in + PreCompact + manual `memory_consolidate`) and the
  rule-based PreCompact/PostCompaction fallback after a provider failure.
  **Fires up to twice per consolidation**: once as an admission *preflight*
  before the LLM call — with an **empty body** and the target page path (for
  multi-page runs, the canonical `sessions/<id>.md` anchor path) — and again
  as the normal write-time check with the real content. Treat blocking calls
  as **decisions, not delivery events**: gate on `op` / `actor` / `workspace`
  / `project` / path, don't reject solely because the body is empty, and
  don't count blocking calls as one-write-happened side effects (use a
  non-blocking observer webhook for that). Any mutation returned during the
  preflight is discarded.
- `delete` — a single page is removed (`Wiki::delete_page`, triggered by the
  `memory_delete_page` MCP tool). Carries the page path, no body; fired
  **before** the file is removed so a mirror can `git rm` the same path. The
  SQLite index is deleted directly by the writer actor; the watcher does not
  reconcile delete events.
- `purge_project` — a whole project is purged (`Wiki::purge_project` →
  `remove_dir_all`, routed from `/admin/purge-project`). Carries the
  project in `ctx`, **no** page path; fired before the directory is
  removed so a mirror can drop the project.
- `purge_session` — one session is purged (routed from
  `/admin/purge-session`). Carries the workspace/project in `ctx`, **no** page
  path — the operation is driven by session id rather than by path. Fired
  before any row is deleted, so a `reject`-policy webhook can refuse the purge
  while the session's data is still intact.
- `purge_workspace` — a whole workspace is purged (routed from
  `/admin/delete-workspace`). Carries the workspace in `ctx.workspace`, leaves
  `ctx.project` empty, and has **no** page path; fired before SQL/file
  destruction for reject-policy admission and dispatched again to non-blocking
  observers after durable work.
- `move_project` — a whole project is moved between workspaces without
  changing `project_id` (fresh-destination true move). Carries the source
  project in `ctx.workspace` / `ctx.project`, destination names in
  `ctx.destination_workspace` / `ctx.destination_project`, and no page path;
  fired before the directory rename + DB re-stamp so a mirror can rename or
  reject the project move.
- `move_session` — one session (its rows and its `sessions/<id>.md` page)
  is moved to another project (`/admin/move-session`). Carries the source
  project in `ctx.workspace` / `ctx.project`, destination names in
  `ctx.destination_workspace` / `ctx.destination_project`, and no page path;
  fired before the page file moves and the DB re-stamps so a mirror can move
  or reject it.
- `handoff_begin` / `handoff_accept` / `handoff_cancel` — a handoff is created,
  consumed, or discarded. Carries the workspace / project and the acting
  operator, no page path (handoffs live in their own table, not the wiki tree).
  Fired by the MCP tools **and** by the automatic hook paths: the SessionEnd
  baton (`handoff_begin`) and the session-start claim (`handoff_accept`).

The three handoff lifecycle ops are dispatched in one fixed order, the same
from every path that raises them: the webhooks that can refuse — `blocking`
with `failure_policy = "reject"` — are awaited **before** the operation, the
operation then runs, and every other subscriber (observers, blocking or not) is
dispatched fire-and-forget **after** it, only if it happened. So no observer is
told about an `accept` that found no pending handoff or a `cancel` another
operator's ownership refused — an `accept` with nothing pending is not
announced to a decider either, since the engine knows there is nothing to
accept before it asks — and an observer subscribed to one of these ops sees the
same event whether it came from an MCP tool or the hook ingress. (`write_page`
and the notification ops below are unchanged: there, a `blocking` webhook is
awaited up front whatever its `failure_policy` — on `write_page` because it can
still mutate the page.)

What a `reject` costs the caller depends on **which path** raised the op, and
the difference is deliberate. On the MCP tools — `memory_handoff_begin`,
`memory_handoff_accept`, `memory_handoff_cancel` — a refusal aborts the tool
call and is returned to the caller as a JSON-RPC error, exactly like
`write_page`: there is a caller who asked for the operation, so it is told the
operation did not happen. On the automatic paths there is no such caller, so a
refusal degrades the lifecycle event instead of failing it:

- SessionEnd asks before `end_session` commits; a refusal skips the baton, is
  logged, and the session page / opt-in consolidation / auto-commit still run.
  The session ends without a handoff rather than ending with no summary at all.
- The session-start claim is served by the synchronous hook path. The shortest
  shipped caller is the shell hook's one-second curl deadline (native hook
  commands allow three seconds), so the server caps admission at 750 ms. A
  refusal, that server deadline, a per-webhook timeout or an unreachable host
  leaves the handoff **open** for the next session; it never fails the session
  start or consumes context after the caller has disconnected.

Budgeting the session-start claim: the deciding webhooks are awaited
**sequentially**, and the chain stops at the first refusal, so what the operator
waits for in the worst case is the **sum** of the `timeout_ms` of every
`reject`-policy webhook subscribed to `handoff_accept` — not the largest of them.
That sum is then capped by the server's 750 ms automatic-claim deadline.
`timeout_ms` still defaults to 2000 ms per webhook for ordinary MCP and write
paths; on automatic session-start acceptance, a chain that cannot approve
within 750 ms is treated as a refusal and the baton remains open. Configure
smaller per-webhook values when you need logs to identify which decider timed
out rather than the aggregate deadline firing first.

`delete` / `purge_project` / `purge_workspace` / `move_project` / `move_session` are notifications — there is no
body to mutate; a `Reject`-policy webhook still aborts the operation
(admission fires BEFORE the SQL destruction in both the `/admin/purge-project`
and `/admin/delete-workspace` paths, and before source teardown in
`/admin/move-project` copy-purge paths, so reject leaves the source intact).
Each webhook opts into the ops it cares about via `events`; the chain checks the
op against `WebhookConfig::events` before dispatching.

Forget-sweep decay eviction uses the same conditional `delete` notification
before removing the Markdown file and writing its tombstone. Aged cleanup
never removes a file: if Markdown has reappeared at the path, it is preserved
and reindexed before only the old SQLite version chain is removed. There is no
file-mirror event for that DB-only history purge.

A copy-purge `/admin/move-project` fires **two** webhook events from
one request: one or more `write_page` notifications as the pages copy
into the destination, then one terminal `purge_project` notification
when the source is torn down. The `purge_project` event carries
`partial_failure: true` if the SQL purge committed but the on-disk
dir removal failed afterwards.

`/admin/delete-workspace` emits `purge_workspace`. Its final async observer
notification carries `partial_failure: true` if the SQL workspace cascade
committed but `<wiki_root>/<workspace_id>` could not be removed.

During the copy leg, the engine skips only the webhook named exactly
`contributors`. Move copies preserve page frontmatter verbatim, including any
existing `contributors` list, so re-running that enrichment hook for every page
adds bulk-move latency without changing the copied page. Other `write_page`
webhooks, such as a `git-mirror`, still run normally for each copied page; the
terminal `purge_project` notification is unchanged.

### What does NOT fire the chain (by design)

- **`log.md` / `log-YYYY-MM.md` appends** — written on every hook event
  (per prompt/tool-call). Routing each through the chain would mean an
  HTTP POST per observation, violating the fire-and-forget hook budget. The
  per-event log is a local audit artifact; back it up out-of-band (batched
  rsync), not per-line.
- **The page bodies behind a handoff** — the `handoff_*` ops above carry only
  the scope and the acting operator. A handoff is SQLite rows, so a file mirror
  has no file change to reconcile from one; what it gets is the lifecycle
  event, not a page.
- **Aged decay-history cleanup** — only the old SQLite version chain changes;
  any Markdown file at the path remains authoritative and is reindexed first
  when necessary. Initial decay eviction and frontmatter TTL expiry use the
  conditional `delete` admission path.
- **`rename-project`** — a `projects.name` column update; the on-disk path
  is the stable UUID, so no file moves and nothing to propagate.
- **`rename-workspace`** — a `workspaces.name` column update plus refreshed
  `_meta.md` manifests; workspace paths are stable UUIDs, so no file move
  notification is needed.
- **External / manual edits on disk** — reconciled by the watcher, not the
  admission chain (the chain is for the engine's own write path).

## 3. Wire contract

### Request (engine → webhook)

```http
POST <webhook.url>
Content-Type: application/json
X-Memory-Op: write_page | consolidate | delete | purge_project | purge_workspace | move_project
             | move_session | handoff_begin | handoff_accept | handoff_cancel
```

(The header is one of those ten values; the second line is a continuation of
the list, not a second header.)

```jsonc
{
  "page": {
    "path": "gotchas/example.md",            // relative wiki path (PagePath)
    "frontmatter": { "title": "...", ... },  // arbitrary JSON, may be null
    "body": "..."                            // markdown body, no frontmatter block
  },
  "ctx": {
    "workspace": "default",                  // resolved name (see §5)
    "project": "ai-memory-ops",              // resolved name
    "destination_workspace": "archive",       // move_project / move_session only; omitted otherwise
    "destination_project": "ai-memory-ops",   // move_project / move_session only; omitted otherwise
    "actor": {                               // request-layer identity
      "agent": "claude-code",                // claude-code | codex | opencode | hook | cli | …
      "user": "djalmajr",                    // null when unauthenticated
      "sub": "8f3a-...",                     // JWT sub
      "client": "72836f52-...",              // DCR client UUID
      "session_id": "019e6d-..."
    },
    "op": "write_page",                      // write_page | consolidate | delete | purge_project | purge_workspace | move_project | move_session
    "partial_failure": true                  // purge_project/purge_workspace only, and ONLY when set
                                             //   (skipped on the wire when false).
                                             //   true → the DB rows were purged but
                                             //   `remove_project_dir` failed afterwards;
                                             //   a filesystem-tracking mirror (git push)
                                             //   should refuse to drop its own copy.
  }
}
```

The `WebhookRequestBody` / `WebhookPagePayload` / `ActorContext` /
`AdmissionContext` types in
`crates/ai-memory-wiki/src/admission.rs` are the authoritative
serialisation source.

### Response (webhook → engine)

| Status | Body | Behaviour |
|---|---|---|
| `200 OK` | `{ "page": { "frontmatter": ..., "body": ... } }` — both inner fields optional. Anything missing means "leave that field unchanged". | The engine swaps in the returned values before the next webhook (or the final atomic write). |
| `204 No Content` | (empty) | The engine treats the webhook as a pure observer / side-effect — no mutation, no parse. |
| `4xx` / `5xx` | (optional textual body, logged) | See **§4 Failure policy**. |

The engine bounds the response read at `MAX_RESPONSE_BYTES` (1 MiB).
Anything beyond that is treated as a no-op with a `warn` log — webhooks
have no legitimate reason to return more than the page envelope.

## 4. Failure policy

Each webhook picks one when the engine can't reach it or it returns
non-2xx:

- **`ignore` (default, recommended)** — Engine logs a `warn` and
  continues with the unmutated page. The page write still succeeds.
  This is the right choice for everything except safety-critical
  enforcers.
- **`reject`** — Engine aborts the write, propagating the error up to
  the caller. Use this **only** when the webhook is a hard precondition
  for persistence (e.g. a future `validate-no-secrets` enforcer).

A webhook that subscribes to multiple ops uses the same policy across
all of them — including the handoff lifecycle ops above, where "abort" means
"decline this handoff", not "fail the event".

## 5. Workspace / project names

The engine resolves `workspace_id` and `project_id` into the same
human-readable names the UI and on-disk wiki use, so webhooks can
address pages by name without re-implementing UUID lookup. Resolution
happens just before the chain fires; both fields are empty when the
wiki was built without [`Wiki::with_store_reader`] (e.g. legacy
embedders, tests that wire a chain without a reader).

External webhooks should treat the names as opaque strings (workspace /
project values use the same validation as `--workspace` / `--project`
CLI flags). They are stable for the lifetime of the workspace / project
— the engine doesn't rename them silently. `rename-project` is a manual
op that an operator runs and that will eventually trigger a fresh
webhook fan-out if you wire one.

## 6. Loop prevention

A webhook that turns around and writes back to the engine (e.g. via
`/admin/write-page`, `memory_write_page`, or the hook ingress) must include the
header

```
X-Memory-Skip-Admission-Chain: <name>[,<name>...]
```

on its re-entrant call. The engine matches the CSV against
`WebhookConfig::name` and short-circuits those hooks for that write, but only
for trusted re-entry (root/auth-disabled requests). Regular DB-user writes
cannot set this header to bypass a reject-policy webhook. Without the skip
header on trusted re-entry, you get infinite recursion (engine → webhook →
engine → webhook → ...). The header propagates only for the single re-entrant
write; the next external write picks the chain back up normally.

## 7. Limits

Constants are exported from the `ai-memory-wiki` crate root:

| Constant | Value | What it caps |
|---|---|---|
| `MAX_ADMISSION_WEBHOOKS` | `16` | Chain length. `AdmissionChain::new` errors out beyond this — a misconfigured template (helm loop, duplicated block) can't push N hooks into the write-path. |
| `MAX_RESPONSE_BYTES` | `1 MiB` | Webhook response body. Beyond this the response is dropped (treated as no-op + `warn`). |
| Per-webhook `timeout_ms` | operator-set (default `2000`) | Single request. The chain is sequential, so total worst case ≈ `Σ timeout_ms`; automatic session-start handoff acceptance additionally caps the whole deciding chain at 750 ms. |

## 8. Configuration

`config.toml`:

```toml
[[admission_webhooks]]
name = "contributors"                                    # stable identifier (used by skip list + logs)
url  = "http://contributors.memory.svc.cluster.local:8080/enrich"
timeout_ms = 2000                                        # per request
failure_policy = "ignore"                                # ignore | reject
events = ["write_page", "consolidate"]
blocking = true                                          # runs synchronously; may mutate / reject

[[admission_webhooks]]
name = "git-mirror"
url  = "http://git-mirror.memory.svc.cluster.local:8080/sync"
timeout_ms = 2000
failure_policy = "ignore"
events = ["write_page", "consolidate", "delete", "purge_project", "move_project"]
blocking = false                                         # fire-and-forget after the write; never blocks it
```

### `blocking` (default `true`)

A webhook is either **blocking** or **non-blocking**:

- **`blocking = true`** (default) — runs *synchronously* inside the write path.
  It can mutate the page (`write_page`/`consolidate`), and a `reject` failure
  aborts the write. The write waits for it (up to `timeout_ms`). Use for
  enrichers/validators (e.g. `contributors`, `validate-no-secrets`).
- **`blocking = false`** — dispatched *fire-and-forget* **after** the durable
  operation has completed. For writes that means the final page is on disk and
  indexed in SQLite; for deletes that means the file and index row are gone;
  for project purges that means the DB purge has completed and filesystem
  removal has been attempted; for project moves it means the directory and DB
  rows now point at the destination workspace. The engine does not wait for it
  and ignores its response, so it **cannot mutate or reject** — it only
  observes/mirrors the final state. Use for pure backups/mirrors (e.g.
  `git-mirror`) so a slow or down sink never adds latency to writes. Still
  honours `events` and the skip list.

Since the blocking chain is sequential, total worst-case write latency is
`Σ timeout_ms` over the **blocking** webhooks only; non-blocking ones add none.

Fire-and-forget dispatch is bounded: **256** such requests may be in flight per
process, and beyond that the request is dropped and logged rather than queued
(the durable operation has already completed, so the caller is never stalled).
On the handoff lifecycle ops — `handoff_begin`, `handoff_accept`,
`handoff_cancel` — only a `reject`-policy webhook is awaited, so a
`blocking = true` hook with `failure_policy = "ignore"` is dispatched
fire-and-forget there and shares that budget: under sustained load it too can be
dropped. Such a drop is logged at ERROR (a non-blocking one at WARN). A hook that
must not be dropped on those ops needs `failure_policy = "reject"`, which makes
it a decider — awaited before the operation, and able to refuse it.

Env override:

```bash
AI_MEMORY_ADMISSION_WEBHOOKS_JSON='[{"name":"contributors","url":"http://contributors.memory.svc.cluster.local:8080/enrich","timeout_ms":2000,"failure_policy":"ignore","events":["write_page","consolidate"],"blocking":true}]'
```

The JSON env var is canonical for webhook lists because the figment env layer
does not reliably reconstruct `Vec<Struct>` from indexed nested variables.

Empty config = no chain attached → zero overhead per write (no client
built, no per-write branch).

## 9. Worked examples

### Mutating: append the writer to `frontmatter.contributors`

```jsonc
// POST /enrich
{
  "page": { "path": "gotchas/x.md", "frontmatter": { "title": "X" }, "body": "..." },
  "ctx":  { "workspace": "default", "project": "ai-memory-ops",
            "actor": { "agent": "claude-code", "user": "djalmajr", "client": "72836f52-..." }, ... }
}

// → 200 OK
{
  "page": {
    "frontmatter": {
      "title": "X",
      "contributors": [
        { "agent": "claude-code", "user": "djalmajr", "client": "72836f52-...",
          "first_seen": "...", "last_seen": "...", "writes": 1 }
      ]
    }
  }
}
```

The engine replaces `frontmatter` with the returned object before
persisting. `body` is left untouched (not in the response).

### Side-effect: mirror the write into an external git repo

```jsonc
// POST /sync (same request body)
// → 204 No Content
```

The webhook materialises the page into a local clone of an external
repo, batches commits, and pushes asynchronously. The engine doesn't
wait for the push — only the local enqueue runs inside the write path
under the webhook's `timeout_ms`.

## 10. Tests

`crates/ai-memory-wiki/tests/admission.rs` covers the wire contract end
to end against an axum loopback server. Categories:

- Mutating frontmatter and body propagates correctly.
- `204` is a no-op (frontmatter / body unchanged).
- `failure_policy=ignore` swallows errors; `failure_policy=reject`
  aborts.
- Multi-webhook chain runs in declared order; each sees the previous
  mutation.
- `X-Memory-Skip-Admission-Chain` short-circuits named hooks.
- `X-Memory-Op` header is set correctly per op.
- `op`-filtering: webhooks only fire on subscribed events.
- `MAX_ADMISSION_WEBHOOKS` rejection at construct time.
- `MAX_RESPONSE_BYTES` cap drops oversized responses.
- `workspace` / `project` resolution propagates into the payload.

`crates/ai-memory-wiki/src/wiki.rs::tests::write_page_resolves_workspace_and_project_names_for_chain`
covers the integrated path (`Wiki::write_page` → store reader resolution
→ chain → recorded payload).

## 11. Where to look in source (the canonical spec)

| Concept | File:line |
|---|---|
| `AdmissionContext` / `ActorContext` / wire structs | `crates/ai-memory-wiki/src/admission.rs` |
| `AdmissionChain::run` (the hot loop) | `crates/ai-memory-wiki/src/admission.rs` |
| Invocation inside `write_page` (resolution + chain call) | `crates/ai-memory-wiki/src/wiki.rs::Wiki::write_page` |
| Config schema (`[[admission_webhooks]]`) | `crates/ai-memory-cli/src/config.rs::Config::admission_webhooks` |
| Server wiring (`with_admission_chain` + `with_store_reader`) | `crates/ai-memory-cli/src/commands/serve.rs` |
| Header → `ActorContext` mapping (mcp-auth → engine) | `crates/ai-memory-mcp/src/actor.rs` |

## 12. Non-goals (planned iterations, not blockers)

- **Parallel fan-out.** The chain is sequential by design today —
  mutation composition is well-defined that way. A future
  `parallel = true` for side-effect-only webhooks (no body mutation
  expected) is possible but not in scope.
- **Webhook discovery / dynamic registration.** Hooks are operator-named
  in config. A future `/admin/admission-webhooks` POST surface is
  conceivable but explicitly out of scope here — single-tenant config
  is simpler and matches how the engine treats every other extension
  point (`[[etl_sources]]`, etc.).
- **Per-webhook metrics surface.** The chain logs via `tracing` today.
  Surfacing per-webhook counters via `/admin/status` is a natural
  follow-up but lives outside this contract.
