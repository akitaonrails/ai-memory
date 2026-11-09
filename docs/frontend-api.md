# Frontend integration: `/api/v1`

> Read-only JSON API and custom-UI hosting model for building third-party
> frontends against an `ai-memory` server. Added in **v0.6.0** (PR #7).
> Everything below is sourced from the actual route handlers in
> `crates/ai-memory-web/src/routes/api.rs` and the response structs in
> `crates/ai-memory-store/src/reader.rs`. Treat those files as the canonical
> reference if anything here drifts.

## 1. What this surface is (and isn't)

| | What you can do | What you can't do |
|---|---|---|
| `/api/v1/*` | Browse workspaces, projects, pages; read full page markdown + frontmatter + back-links; list a page's stored versions and diff two of them; search (global FTS5, or scoped through `memory_query`'s hybrid ranker, single or multi-project); read the cross-project and in-project link graphs; aggregate "overview" snapshots; drill into stale / duplicate / orphan pages; list a project's sessions and read one session's raw observations. | Write, delete, rename, lint, consolidate, run sweeps, manage handoffs. The `/api/v1` surface is **read-only by construction**: the handlers contain zero writer calls, and `crates/ai-memory-web/clippy.toml` forbids the `Wiki` write and destructive methods anywhere in the crate, so adding one fails `cargo clippy -D warnings`. Writes still go through `/admin/*` (used by the CLI) or MCP tools. |
| `--web-ui-dir` | Host any SPA at `/web` (or `--web-slug`), same-origin with the API, behind the same auth. The default built-in `/web` browser stays the fallback when the flag is absent. | Host the SPA on a *different* origin without a reverse proxy. Use same-origin hosting or configure CORS deliberately (see §9). |

## 2. Auth model

The built-in web navigation also reads `GET /web/inbox/notifications` (replace
`/web` with the configured mount). It returns `pending`, `project_count` and up
to five `projects`, each with `workspace`, `project` and `pending`, filtered by
the viewer's repository access. The response is protected and non-consuming,
uses `Cache-Control: no-store`, and contains no message body or subject.
Notification links open `/web/inbox?workspace=...&project=...&state=pending`.
The optional `state` filter (`pending`, `claimed`, `cancelled`, or empty for
all) is applied before history pagination, and unknown states are refused.
Received mail defaults to `pending` when `state` is omitted, and an empty
`state=` shows all history. Sent mail defaults to all states.

Project and session HTML views accept `calls_q` (a literal model, harness or
response substring, at most 256 characters), `calls_sort=tokens|recent`,
`calls_signal=all|signals` and `calls_offset`. The database applies scope and
ownership, search, signal filtering and ordering before it selects the
fifty-response page. Totals and signal averages still cover all authorized
responses in that scope. These HTML query parameters are separate from
observation pagination, and they add no write capability or new `/api/v1`
endpoint. Project and session usage refresh every ten seconds while visible,
and the search controls and focus stay in place.

`/api/v1/*` and `/admin/*` are dual-auth surfaces:

- Human operators authenticate with `POST /auth/login`. The engine returns an
  `HttpOnly`, `SameSite=Strict` `ai_memory_session` cookie plus a readable
  `ai_memory_csrf` cookie. Browser requests use `credentials: "include"`;
  mutations also copy the CSRF cookie to `X-CSRF-Token`.
- Machine clients send `Authorization: Bearer <token>`. The static
  `AI_MEMORY_AUTH_TOKEN` is machine-root authority; native `aim_` API keys are
  always User-level. Bearers never authenticate `/auth/*`.
- A recognized Bearer has precedence over every browser credential. An invalid
  Bearer fails closed rather than falling back. Before human auth activates,
  deprecated Basic/cookie compatibility is evaluated for GET requests; after
  activation, Basic and unknown schemes do not suppress a valid web session.
- `/mcp`, `/identity`, hooks, handoffs, and workstream routes are machine-only. A web-session
  cookie cannot authenticate them.
- A disallowed `Host` header receives `403 Forbidden` before auth evaluation
  (DNS-rebinding guard).

The custom SPA shell and its static assets are public so the login screen can
load. Its data routes remain protected. When `--web-ui-dir` is **absent**, the
builtin wiki exposes public `GET {web_slug}/login` and
`GET {web_slug}/change-password` HTML pages that call the same `/auth/*`
endpoints; navigational browser GETs to protected wiki routes redirect there
instead of returning bare JSON 401/403. `/api/v1` stays JSON either way.
The builtin wiki also serves `GET {web_slug}/w/{workspace}/{project}/h/{*path}`
(a page's version history and diff, the HTML twin of §4.13) and
`GET {web_slug}/w/{workspace}/{project}/s/{session_id}` (a session's
observations in capture order, a page at a time, through the same readers,
visibility rule and caps as §4.11 and §4.12; `offset`, `limit` clamped
`1..=200`, default `50`, and `at` to jump to the page holding a 1-based
position). Both follow the same per-project read rule as the page view.
The builtin wiki also serves `GET {web_slug}/graph` (the HTML twin of
§4.9: a circular SVG of projects plus a table of page-level edges) and
`GET {web_slug}/w/{workspace}/{project}/graph` (the HTML twin of §4.9.1).
The builtin wiki also serves the root-only `GET {web_slug}/pending` page. It
lists pending auto-improvement proposals and posts approve and reject to the
existing `/admin/pending-writes/*` routes. A signed-in non-root user gets a
403 page, not the change-password redirect. A browser on the GET-only legacy
Basic credential sees each proposal's `ai-memory pending-writes` commands in
place of the buttons, since its posts would be refused. The root-only
`GET {web_slug}/health` page follows the same gate: hook counters (accepted,
shed, 429), writer queue, the spool and recovery journal under the server's
data directory, the consolidation queue, embedder and provider state, and the
failing server-side checks with their fix commands. It is HTML only, answers
`Cache-Control: private, no-store`, and shows counts, timestamps and configured
labels, never provider error text or page content.
`GET {web_slug}/search?q=…&workspace=…&project=…&explain=1` is the project
search: the same hybrid ranker and per-project read rule as a scoped
`/api/v1/search` (§4.5), and with `explain=1` a recall trace (per-hit stream
ranks, raw scores, RRF contributions, fused score, authority, intent boost,
rerank score, plus per-stream timings). `limit` clamps to `1..=100`, default
`20`; without a scope it is the cross-project FTS5 search (default `50`), whose
trace is the FTS time alone. Start
with:

```http
GET /auth/me
```

An authenticated response includes the operator identity, password-change
state, and server-calculated capabilities. On an intentionally zero-config
loopback server it instead returns an explicit anonymous snapshot. When human
authority is configured, login is:

```http
POST /auth/login
Content-Type: application/json

{"username":"alice","password":"…"}
```

`/auth/login` and `/auth/recovery` answer 415 to any other content type, so a
form post or a `text/plain` request from another site cannot reach them.

Do not put `AI_MEMORY_AUTH_TOKEN`, `aim_` keys, session values, or CSRF values
in `localStorage`. Before any human password or completed bootstrap exists,
deprecated GET-only browser compatibility may accept the root bearer through
HTTP Basic, or an HttpOnly `ai_memory_auth` cookie an older build set; a Basic
sign-in no longer sets that cookie. Human activation disables
that path immediately. See [`docs/users.md`](users.md) for bootstrap, password
rotation, recovery, roles, session expiry, and API-key lifecycle.

Machine clients can call `GET /identity` with their bearer key, even when the
web UI is disabled. It reports only the authenticated caller and server version:

```json
{"version":"2.5.2","level":"user","operator":"user:alice","distinguishes_operators":true}
```

`level` is `root`, `user` or `anonymous`; `operator` is the qualified identity
key, or `null` on an unauthenticated single-user server. Web-session cookies
cannot authenticate this route. Its response uses `Cache-Control: private,
no-store`. See [users.md](users.md) for machine keys and identity types.

## 3. Error model

All errors return a JSON body of shape:

```json
{ "error": "human-readable message" }
```

with one of these statuses:

| Status | When |
|---|---|
| `400 Bad Request` | invalid query params, malformed input, partial scope (workspace without project or vice versa), too many scopes in `POST /search` (>25), empty `q`, malformed session id, unknown observation `kinds` or `order`. |
| `401 Unauthorized` | human session or machine Bearer is missing, expired, revoked, or invalid. |
| `403 Forbidden` | Host header not in allowlist; cookie mutation lacks valid CSRF; password change is pending; or the authenticated authority lacks the requested capability. |
| `404 Not Found` | workspace, project, or page doesn't exist; page file missing on disk; or a session id that is not visible in that project for the caller. |
| `429 Too Many Requests` | login/recovery rate limit or bounded password-KDF queue is saturated. |
| `500 Internal Server Error` | reader pool / SQLite failure. Body is always a fixed generic error; the underlying cause is logged server-side rather than returned, so it cannot leak paths or configuration to a browser. |

## 4. Endpoint reference

All endpoints are `GET` unless noted. Paths under `/api/v1/`.

### 4.1 Workspaces

```http
GET /api/v1/workspaces
```

**Response:** a bare JSON array, `[WorkspaceSummary, …]`

```json
[
  {
    "workspace_name": "default",
    "project_count": 3,
    "page_count": 412,
    "last_updated": "2026-05-28T14:02:11.123Z"
  }
]
```

`last_updated` is `null` for an empty workspace.

### 4.2 Projects

```http
GET /api/v1/projects                  # all projects across all workspaces
GET /api/v1/projects?workspace=NAME   # projects in one workspace
```

**Response:** a bare JSON array, `[ProjectSummary, …]`

```json
[
  {
    "workspace_id": "0193e7a1-0000-7000-8000-00000000000a",
    "workspace_name": "default",
    "project_id": "0193e7a1-0000-7000-8000-00000000000b",
    "project_name": "ai-memory",
    "page_count": 138,
    "last_updated": "2026-05-28T14:02:11.123Z"
  }
]
```

`project_id` is the stable address of a project: names can collide (a legacy
twin row can carry another project's old name), and the admin project
operations accept the id where the name is ambiguous. A restricted project's
row, id included, is hidden from a caller without a grant.

### 4.3 Pages (list)

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/pages
```

**Response:** a bare JSON array, `[PageSummary, …]`

```json
[
  {
    "path": "decisions/0007-db.md",
    "title": "Standardised on Postgres",
    "kind": "decision",
    "tier": "semantic",
    "updated_at": "2026-05-27T09:12:00.000Z"
  }
]
```

`404` if the workspace or project doesn't exist. A page whose `expires_at`
TTL has passed is left out, as search and `recent` leave it out; the
single-page read below still returns it.

### 4.4 Page (read full)

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/pages/{*path}
```

Wiki path is a wildcard: `decisions/0007-db.md`, `concepts/foo/bar.md`,
etc. Returns merged metadata + body markdown + frontmatter + resolved
links + back-links.

**Response (flat object):**

```json
{
  "id": "01a11da7-0604-7cc1-b518-1f0e0f2d4705",
  "project": "ai-memory",
  "path": "decisions/0007-db.md",
  "title": "Standardised on Postgres",
  "kind": "decision",
  "tier": "semantic",
  "pinned": true,
  "created_at": "2026-05-27T09:12:00.000Z",
  "updated_at": "2026-05-28T11:04:33.123Z",
  "supersedes": null,
  "frontmatter": { "tags": ["adr"], "pinned": true },
  "body_markdown": "# Standardised on Postgres\n\n…",
  "links":     [ { "path": "concepts/db-rules.md", "title": "DB rules", "kind": "rule" } ],
  "backlinks": [ { "path": "sessions/2026-05-27.md", "title": "Session 2026-05-27", "kind": "session" } ]
}
```

`id` is the latest version's id. It changes with every new version, so it is
the token to pass back as `expected_page_id` to a conditional
`memory_write_page` or `memory_delete_page`.

`created_at` is when the page's oldest retained version was written, so it
stays put across rewrites; `updated_at` is when the latest version was written.

`404` for missing workspace/project, missing page row, or missing file
on disk (the body is read from the markdown file at request time).

### 4.5 Search

Two forms: query-string for the common single-scope or global case, and a
JSON body for multi-scope.

```http
GET /api/v1/search?q=karpathy&limit=20                                # global
GET /api/v1/search?q=karpathy&workspace=default&project=ai-memory     # one project
```

```http
POST /api/v1/search
Content-Type: application/json

{
  "q": "karpathy",
  "scopes": [
    { "workspace": "default", "project": "ai-memory" },
    { "workspace": "default", "project": "shared-notes" }
  ],
  "limit": 20
}
```

**Response:** a bare JSON array, `[SearchHit, …]`

```json
[
  {
    "workspace": "default",
    "project": "ai-memory",
    "path": "concepts/karpathy-wiki.md",
    "title": "Karpathy LLM Wiki pattern",
    "kind": "concept",
    "snippet": "Andrej <mark>Karpathy</mark>'s LLM wiki design …",
    "rank": -8.4
  }
]
```

Rules:

- `q` is required and non-empty (400 otherwise).
- `limit` is clamped to `1..=100`. Default `10`.
- Partial scope is **rejected** with `400` (passing only `workspace` or
  only `project` to keep scoping unambiguous).
- `scopes` (POST) is capped at `25` entries; can't be combined with
  top-level `workspace`/`project`.
- `snippet` contains FTS5 HTML markers (`<mark>…</mark>`) around the
  matched terms.
- A **global** search (no scope) is FTS5 over every project the caller may
  read, like `memory_query(global=true)`; `rank` is the FTS5 rank.
- A **scoped** search (`workspace`+`project`, or `scopes`) runs the hybrid
  ranker `memory_query` uses for a project: FTS5, entity, vector (when the
  server has an embedder), graph and observation streams fused by RRF, the
  bounded authority adjustment, and the optional reranker. The same query
  returns the same order as `memory_query`; `rank` is the negated,
  authority-adjusted fused score. Several `scopes` merge by best rank per
  page.
- `rank`: **lower is better** in both modes.

### 4.6 Recent

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/recent?limit=20
```

`is_latest = 1` pages whose `expires_at` TTL has not passed, ordered by
`updated_at` DESC by instant, with path ascending for equal timestamps.
Fractional-second precision in the serialized timestamps does not change this
order. `limit` clamped `1..=100`, default `10`.

Every reader surface uses the same `kind` contract. An explicit frontmatter
`kind` wins; otherwise the path families `_rules/`, `_slots/`, `sessions/`,
`decisions/`, `gotchas/`, `concepts/`, `procedures/`, and `notes/` derive
`rule`, `slot`, `session`, `decision`, `gotcha`, `concept`, `procedure`, and
`note`, respectively. Other paths fall back to `fact`.

**Legacy response:** a bare JSON array of `PageSummary` objects.

```json
[
  {
    "path": "sessions/2026-05-28.md",
    "title": "Session 2026-05-28",
    "kind": "session",
    "updated_at": "2026-05-28T14:02:11.123Z"
  }
]
```

Supply `updated_since` (RFC 3339) or `cursor` to opt into incremental paging:

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/recent?updated_since=2026-09-30T00%3A00%3A00Z&limit=20
```

```json
{"pages": [], "next_cursor": null}
```

The cutoff is exclusive (`updated_at > updated_since`). Results are ordered
by `(updated_at, path)` ascending and fetched with a bounded SQL query. Pass
`next_cursor` unchanged to continue; an optional cutoff must match the cursor's
cutoff. Cursors use versioned URL-safe base64 without padding, are opaque to
clients, and are limited to 8192 characters (6144 decoded bytes). They are bound
to the workspace and project. Invalid, foreign-scope or conflicting cursors
return 400. Each request rechecks authorization and uses
`Cache-Control: private, no-store`.

Incremental results omit superseded and expired pages, including expired pinned
pages. They provide no snapshot or deletion feed: concurrent changes can appear
on a later page, and deleted or expired pages are absent. Calls without either
parameter retain the legacy array, descending order and cache behavior.

### 4.7 Briefing (structured snapshot)

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/briefing?limit=10
```

Same payload `memory_briefing` returns: counts + activity windows +
last-observation + open handoffs + `_rules/` + `_slots/` + N most-recent
pages. No LLM, deterministic.

**Response:** `BriefingSnapshot`

```json
{
  "counts": {
    "pages_latest": 138,
    "pages_all": 162,
    "sessions": 27,
    "observations": 4198
  },
  "activity_7d":  { "days": 7,  "sessions": 6,  "observations": 921,  "pages_updated": 41 },
  "activity_30d": { "days": 30, "sessions": 24, "observations": 3712, "pages_updated": 102 },
  "last_observation_at": "2026-05-28T13:58:02.123Z",
  "pending_handoff_count": 0,
  "rules": [{ "path": "_rules/postgres.md", "title": "Postgres only", "kind": "rule",  "updated_at": "…" }],
  "slots": [{ "path": "_slots/focus.md",    "title": "Current focus", "kind": "slot",  "updated_at": "…" }],
  "recent_pages": [
    { "path": "sessions/2026-05-28.md", "title": "Session 2026-05-28", "kind": "session", "updated_at": "…" }
  ]
}
```

### 4.8 Overview (workspace + project aggregates)

```http
GET /api/v1/workspaces/{workspace}/overview?limit=10
GET /api/v1/workspaces/{workspace}/projects/{project}/overview?limit=10
GET /api/v1/workspaces/{workspace}/projects/{project}/handoffs?state=open&limit=50
GET /api/v1/workspaces/{workspace}/projects/{project}/handoffs?all_owners=true
```

### Handoff listing

`state` accepts `open` | `accepted` | `expired`; omit it to list every state,
which is how you find a baton that was already consumed. Results are scoped by
owner: an authenticated caller sees their own plus the shared handoffs, an
anonymous browser sees only shared ones. An owned handoff (and the prompt-
derived text inside it) is never rendered to someone it does not belong to.
The same scoping applies to the `handoff` field of both overview endpoints and
to `pending_handoff_count`, so the count and the fetch always agree.
For recovery, a root-authorized request may pass `all_owners=true` to list all
operators' rows. User and anonymous requests receive `403`; the default remains
own plus shared, including for root.

On a server that authenticates, the listing's prompt-derived fields
(`summary`, `open_questions`, `next_steps`) are served to a caller the server
can name and to the root operator; an automatic handoff synthesises them
verbatim from the operator's prompts, and the listing returns the project's
whole history rather than the single newest open row. A caller that is neither
named nor root gets the fields absent and `redacted` set to `true`; the metadata
(state, timestamps, agent, cwd, touched files, ownership) is always served. A
server with no auth configured serves the bodies, since it already serves every
page body unauthenticated.

"Can name" means the identity the auth tier itself resolved
(`ActorContext::identity_key()`): the asserted issuer/subject pair when there is
one, otherwise the username. An ingress that terminates OIDC and forwards both
`X-Memory-Actor-Issuer` and `X-Memory-Actor-Sub` therefore reads its own
handoffs and the shared ones with `redacted: false`, and no rung of the auth
chain produces an
authenticated-but-unnameable caller today. The redacting arm is a fail-safe
floor, not a live tier. `owner` / `accepted_by` carry the qualified storage key
(`user:alice`, `oidc:<issuer-byte-length>:<issuer><subject>`).

```json
{
  "handoffs": [
    {
      "id": "01930…",
      "agent": "claude-code",
      "at": "2026-07-28T12:00:00Z",
      "state": "accepted",
      "summary": "…",
      "open_questions": [],
      "next_steps": [],
      "redacted": false,
      "files_touched": [],
      "owner": "user:alice",
      "accepted_by": "user:alice",
      "accepted_at": "2026-07-28T13:00:00Z"
    }
  ]
}
```

Bundles what a frontend usually needs on its home view in one round-trip.

**Workspace overview** returns the latest open handoff across the workspace,
plus `briefing` and `health` aggregated across all of its projects:

```json
{
  "handoff":  { "agent": "claude-code", "at": "…", "project": "ai-memory", "summary": "…", "open_questions": [ … ], "next_steps": [ … ] },
  "briefing": { "counts": { … }, "activity_7d": { … }, "rules": [ … ], "recent_pages": [ … ] },
  "health":   { "stale": 4, "duplicates": 1, "contradictions": 0, "orphans": 12,
                "audited_at": null, "stale_pages": [HealthPage, …],
                "duplicate_pages": [ … ], "orphan_pages": [ … ] }
}
```

**Project overview** uses the same response shape, scoped to that project. In
either response, `handoff` is `null` when no open handoff matches the scope:

```json
{
  "handoff":  { "agent": "claude-code", "at": "…", "project": "ai-memory", "summary": "…", "open_questions": [ … ], "next_steps": [ … ] },
  "briefing": { … },
  "health":   { … }
}
```

`HealthPage`:

```json
{
  "workspace": "default",
  "project": "ai-memory",
  "path": "concepts/old-thing.md",
  "title": "Old thing",
  "kind": "concept"
}
```

> Note: `handoff` is **not** consumed by the read API. The
> handoff stays "open" and can still be accepted by the next agent.

### 4.9 Cross-project graph

```http
GET /api/v1/graph
GET /api/v1/graph?workspace={workspace}&project={project}
```

Returns every resolved wikilink whose endpoints sit in different
projects, with both endpoints' workspace + project + path. Useful for
rendering a project-level dependency view in the SPA. Omit the query
string for the whole graph. `workspace` and `project` must be provided
together and keep only edges that touch that project; passing only one
of them is `400`. An unknown or unreadable project is the same refusal
as the other scoped routes (`404` / `403`). With authorization on, an
edge still requires both ends to be readable.

```json
{
  "edges": [
    {
      "from_workspace": "default",
      "from_project":   "ai-memory",
      "from_path":      "decisions/0014-storage.md",
      "to_workspace":   "default",
      "to_project":     "infra",
      "to_path":        "runbooks/sqlite-wal.md"
    }
  ]
}
```

### 4.9.1 Project link graph

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/graph
```

Latest unexpired pages in that project and the resolved same-project
wikilinks among them. Cross-project and unresolved links are omitted.
When the project has more than 120 live pages, the response keeps the
pages with the most same-project neighbours and then the most recently
updated, and sets `truncated` to `true`. The same per-project read rule
as the page list applies.

```json
{
  "workspace": "default",
  "project": "ai-memory",
  "truncated": false,
  "nodes": [
    { "path": "decisions/0014-storage.md", "title": "Storage", "degree": 2 }
  ],
  "edges": [
    { "from_path": "concepts/wiki.md", "to_path": "decisions/0014-storage.md" }
  ]
}
```

### 4.10 Browser tab icon

```http
GET /favicon.ico
```

Returns the same transparent PNG the built-in web UI serves as the
header logo. Browsers fetch this path automatically. The route is
present whenever the web UI is enabled (`--enable-web`) and is
mounted at the absolute host root, outside `--base-path` and outside
the `/web` nest, so the browser's automatic fetch reaches it even
under a subpath deployment. The response is `image/png` despite the
`.ico` URL (modern browsers accept PNG icons), and the route is
**exempt from authentication and host allowlist**: a fresh tab can fetch the
icon before login, and the embedded PNG is the same one any visitor to `/web`
already sees, so the info-leak surface is nil.

### 4.11 Sessions

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/sessions?limit=20&offset=0&include_open=false
```

Sessions that touched the project, newest first: a session is listed when its
row is anchored in the project OR at least one of its observations landed
there, so a session that changed repositories mid-flight shows up in both.
`observation_count` counts only this project's rows. `limit` clamped
`1..=100`, default `20`; `offset` default `0`; `include_open` default
`false` (only effectively ended sessions). Substantive observations in the
requested project after the persisted end make the read view open the session
again: `ended_at` is `null`, and `include_open=false` excludes it.
Lifecycle-only observations do not reopen it. The stored end stays the writer's
finalization watermark, and this read changes neither history nor handoff
admission. Owner-filtered like handoffs: a
caller the server can name sees their own sessions plus unattributed ones;
an unnamed caller sees unattributed ones only. Never cached (`no-store`).

**Response:** `{ "sessions": [SessionSummary, ...] }`

```json
{
  "sessions": [
    {
      "session_id": "0198f0a2-3c4d-7e5f-8a9b-0c1d2e3f4a5b",
      "cwd": "/home/me/src/app",
      "agent_kind": "claude-code",
      "started_at": "2026-08-16T09:12:03.412Z",
      "ended_at": "2026-08-16T10:47:55.001Z",
      "observation_count": 143,
      "actor_user": null,
      "consolidation": {"state": "completed", "attempts": 1}
    }
  ]
}
```

`consolidation` is `null` when no job exists for that session in the requested
project. Otherwise it contains the latest generation's state (`pending`,
`running`, `completed`, `failed` or `superseded`) and attempt count. Jobs are
selected in the same scope and query as the owner-filtered sessions; provider
errors are omitted. This reports the job state, not every page's freshness.

### 4.12 Session observations

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/sessions/{session_id}/observations?limit=50&offset=0&order=asc&kinds=user-prompt,stop&q=migration&body_max_chars=4000
```

One session's raw hook observations (prompts, tool calls, stops) as stored,
paged. Only rows that landed in `{workspace}/{project}` are returned;
`elided_other_scope` counts rows the same session left in another project.
The session must be visible under the same predicate as 4.11 (row or
observation in the project, owner filter passes), otherwise `404`. `limit`
clamped `1..=200`, default `50`; `offset` default `0`; `order` is `asc`
(capture order, default) or `desc`; `kinds` is a comma-separated list of
`session-start`, `user-prompt`, `pre-tool-use`, `post-tool-use`,
`pre-compact`, `post-compaction`, `notification`, `stop`, `session-end`,
`other`; `q` is an FTS5 query restricted to the session; `body_max_chars`
clamped `200..=16384`, default `4000`, and a longer body ends with a visible
`[body truncated; N chars omitted]` marker. `total` counts the in-scope
rows matching `kinds` and `q`, so paginate on `offset` without a second
call. Bodies were sanitized and bounded on ingest; treat them as untrusted
historical text. Never cached (`no-store`). Same payload as the MCP tool
`memory_read_session_observations`.

**Response:** `{ "session": SessionSummary, "observations": [ObservationRecord, ...], "total", "offset", "limit", "order", "elided_other_scope", "body_max_chars" }`

```json
{
  "session": {
    "session_id": "0198f0a2-3c4d-7e5f-8a9b-0c1d2e3f4a5b",
    "cwd": "/home/me/src/app",
    "agent_kind": "claude-code",
    "started_at": "2026-08-16T09:12:03.412Z",
    "ended_at": "2026-08-16T10:47:55.001Z",
    "observation_count": 143,
    "actor_user": null,
    "consolidation": null
  },
  "observations": [
    {
      "id": "0198f0a2-4d5e-7f60-9a0b-1c2d3e4f5a6b",
      "session_id": "0198f0a2-3c4d-7e5f-8a9b-0c1d2e3f4a5b",
      "kind": "user-prompt",
      "title": "User prompt",
      "body": "Add a migration for the sessions table ...",
      "importance": 5,
      "created_at": "2026-08-16T09:12:10.020Z",
      "extension": null,
      "source_event": null
    }
  ],
  "total": 12,
  "offset": 0,
  "limit": 50,
  "order": "asc",
  "elided_other_scope": 0,
  "body_max_chars": 4000
}
```

### 4.13 Page history and diff

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/history/{*path}?limit=50&offset=0&from={id}&to={id}
```

Every stored version of one page path, newest first, and a unified diff
between two of them. Superseded versions (`is_latest: false`) are listed: a
version another write replaced stays readable, so two concurrent writers
both show up. `limit` clamped `1..=200`, default `50`; `offset` default `0`;
`total` counts every version at the path. `to` defaults to the newest
version, `from` to the version `to` replaced (none for a first version, which
then diffs against empty). Both are version ids from this listing (the `id`
also returned by §4.4). An id from another project, workspace or path is a
`404`, the same as an unknown one; a malformed id is a `400`. A path with no
versions is a `404`. Expired versions follow the TTL rule of every other read:
a version whose `expires_at` has passed is not listed and its id is a `404`,
and a page whose live version expired has no history at all (`404`), because
the next forget sweep deletes every row at its path. A `supersedes` link to a
hidden version is `null`. Never cached (`no-store`).

The diff is bounded. A side larger than 512 KiB or 10,000 lines, or a pair
more than 1,000 changed lines apart, returns `status: "too_large"` with a
`reason` and no hunks. Otherwise `hunks` carries at most 2,000 lines, with
three lines of context, and `truncated: true` when the cap cut it short.
Bodies are untrusted page text; render them escaped.

**Response:**

```json
{
  "workspace": "default",
  "project": "app",
  "path": "notes/plan.md",
  "total": 2,
  "offset": 0,
  "limit": 50,
  "versions": [
    {
      "id": "01a1…b2",
      "supersedes": "01a1…a7",
      "is_latest": true,
      "title": "Plan",
      "tier": "semantic",
      "created_at": "2026-10-12T09:12:00.000Z",
      "body_chars": 412,
      "author": { "username": "carol" }
    },
    {
      "id": "01a1…a7",
      "supersedes": null,
      "is_latest": false,
      "title": "Plan",
      "tier": "semantic",
      "created_at": "2026-10-11T17:40:12.000Z",
      "body_chars": 380
    }
  ],
  "diff": {
    "from": { "id": "01a1…a7", "title": "Plan", "created_at": "2026-10-11T17:40:12.000Z" },
    "to":   { "id": "01a1…b2", "title": "Plan", "created_at": "2026-10-12T09:12:00.000Z" },
    "status": "changed",
    "truncated": false,
    "hunks": [
      {
        "old_start": 3, "old_len": 2, "new_start": 3, "new_len": 2,
        "lines": [
          { "op": " ", "text": "shared line" },
          { "op": "-", "text": "revision 0" },
          { "op": "+", "text": "revision 1" }
        ]
      }
    ]
  }
}
```

`author` is omitted for an unattributed write. `status` is `same`, `changed`
or `too_large`; `from` is `null` when `to` replaced nothing at this path.

## 5. Limits and pagination

- Most `limit` query params clamp to `1..=100`; handoff history, session
  observations and page history clamp to `1..=200`. Session listing and session
  observations take an `offset`; observations also report `total`.
- Session observation bodies are capped per row by `body_max_chars`
  (`200..=16384`, default `4000`) with a visible truncation marker.
- `POST /api/v1/search`: at most **25 scopes** per request.
- HTTP body cap: **10 MB** (shared with the MCP body limit; you won't
  hit this for normal API traffic).
- **Cache-Control + ETag.** Every `/api/v1` read is grant-filtered (#708):
  what it returns depends on who is signed in, so none of them may be served
  from the browser's own cache to a later viewer at the same URL. The list,
  graph and recent reads (workspaces, projects, pages, graph, recent) and the
  actor-scoped briefing, overview, handoff-list, session-list, session
  observation and page-history reads all use `Cache-Control: private,
  no-store`. The single-page read carries a SHA-256 `ETag` and uses
  `Cache-Control: private, no-cache`: the browser keeps the body but must
  revalidate with `If-None-Match` every time, and because the server runs the
  grant check before comparing the tag, a viewer without a grant receives `403`
  rather than a `304` from the browser's copy. Search responses are not
  cacheable because the request body affects the result. The effect is that a
  browser cannot reuse Alice's response after credentials at the same URL
  switch to Bob.

## 6. Custom UI hosting and base paths

For an API-only companion, mount the protected JSON surface without a browser
UI:

```bash
ai-memory serve --transport http --enable-api
```

`--enable-web` continues to imply `/api/v1` for compatibility. `--enable-api`
alone does not mount `/web`, its login pages, static assets, or the unauthenticated
favicon route. The API uses the same machine Bearer or browser-session auth gate
in either mode.

```bash
ai-memory serve \
    --transport http \
    --bind 127.0.0.1:49374 \
    --enable-web \
    --web-ui-dir /path/to/your-spa/dist
```

The static directory is served at `/web` via `tower-http::ServeDir`:

- **Public shell, protected data.** `index.html`, client-router fallbacks, and
  static assets load without credentials so the login form can render.
  `/auth/me`, `/api/v1/*`, and `/admin/*` enforce the session/Bearer contract
  above. The built-in wiki router used when `--web-ui-dir` is absent remains
  protected because it renders data server-side.
- **SPA fallback.** Missing paths fall back to `index.html`, so a
  client-side router (React Router, SvelteKit, etc.) can own
  `/web/whatever` without 404s.
- **Path traversal is rejected** by `ServeDir`'s default safety.
- **Pre-startup validation:** the directory must exist *and* contain
  `index.html`, or `ai-memory serve` exits with a clear error before
  binding. Requires `--enable-web` to also be set.
- **Base-path injection:** ai-memory injects `<base href="...">` and
  `<meta name="ai-memory-base-path" content="...">` into the SPA shell. This
  covers direct `/web`, `/web/index.html`, and client-router fallback paths;
  static assets are served unchanged.

When a reverse proxy keeps ai-memory under a URL subpath, set
`--base-path` (or `AI_MEMORY_BASE_PATH`) so every HTTP surface moves together:

```bash
ai-memory serve \
    --transport http \
    --bind 127.0.0.1:49374 \
    --enable-web \
    --base-path /wiki
```

With `--base-path /wiki`, the API lives at `/wiki/api/v1`, MCP at
`/wiki/mcp`, hooks at `/wiki/hook`, admin routes at `/wiki/admin/*`, and the
default web UI at `/wiki/web`. Set `--web-slug /` to mount the web UI or custom
SPA at the base root (`/wiki`) instead of `/wiki/web`.

**Base-path safety rules.** Both `--base-path` and `--web-slug` go
through the same normaliser. Segments must be RFC 3986 unreserved
characters (`[A-Za-z0-9-._~]`). Three things collapse the prefix to
`""` (root mount) with a startup `WARN` so you can see the
downgrade in the log:

- `.` or `..` segments. Their characters are unreserved on their own,
  but at the segment boundary they mean "current" and "parent", so a
  single typo would turn your prefix into a traversal vector.
- Any character outside the unreserved set (spaces, `<`, `"`, etc.).
- Empty / whitespace-only input.

The trailing-slash redirect at `{base_path}{web_slug}/` →
`{base_path}{web_slug}` keeps the query string. Fragments are
client-side and never reach the server.

When `--web-ui-dir` is **absent**, the built-in server-side `/web`
browser is the default (read-only HTML rendering, search,
project tree), unchanged from before.

## 7. Worked example: minimal SPA fetch

```js
// Resolve bases from the SPA shell injected by ai-memory. The meta tag is
// empty at host root and e.g. "/wiki" behind a subpath reverse proxy.
const basePath = document
  .querySelector('meta[name="ai-memory-base-path"]')
  ?.getAttribute("content") ?? "";
const origin = `${location.origin}${basePath}`;
const API = `${origin}/api/v1`;

async function login(username, password) {
  const resp = await fetch(`${origin}/auth/login`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ username, password }),
  });
  if (!resp.ok) throw new Error(`login failed: ${resp.status}`);
  return resp.json();
}

function cookie(name) {
  const prefix = `${encodeURIComponent(name)}=`;
  return document.cookie
    .split(";")
    .map(value => value.trim())
    .find(value => value.startsWith(prefix))
    ?.slice(prefix.length);
}

async function apiGet(path, params) {
  const url = new URL(`${API}${path}`, location.origin);
  if (params) Object.entries(params).forEach(([key, value]) =>
    value != null && url.searchParams.set(key, value));
  const resp = await fetch(url, {
    credentials: "include",
    headers: { Accept: "application/json" },
  });
  if (!resp.ok) {
    const { error } = await resp.json().catch(() => ({ error: resp.statusText }));
    throw new Error(`${resp.status}: ${error}`);
  }
  return resp.json();
}

// Call login() from the UI first. The HttpOnly session is never visible here.
const overview = await apiGet("/workspaces/default/projects/ai-memory/overview", {
  limit: 10,
});
console.log(overview.briefing.counts.pages_latest, "pages");

// POST is CSRF-protected even when the operation is read-only.
const search = await fetch(`${API}/search`, {
  method: "POST",
  credentials: "include",
  headers: {
    "Content-Type": "application/json",
    "X-CSRF-Token": decodeURIComponent(cookie("ai_memory_csrf") ?? ""),
  },
  body: JSON.stringify({
    q: "karpathy",
    scopes: [
      { workspace: "default", project: "ai-memory" },
      { workspace: "default", project: "shared-notes" },
    ],
    limit: 20,
  }),
}).then(response => response.json());
```

`curl` smoke test:

```bash
# Use the bearer already configured on the running server.
TOKEN='<server-configured bearer token>'
curl -fsS "http://127.0.0.1:49374/api/v1/workspaces" -H "Authorization: Bearer $TOKEN" | jq
```

## 8. Where to look in source (the canonical spec)

If a doc/response shape ever conflicts with the code, the code wins.
Read these:

| | Location |
|---|---|
| Route registration + handler bodies | `crates/ai-memory-web/src/routes/api.rs` |
| Response structs (`PageHit`, `WorkspaceSummary`, `BriefingSnapshot`, `HealthPage`, `SessionSummary`, `ObservationRecord`, …) | `crates/ai-memory-store/src/reader.rs` |
| Session listing + per-session observation readers (`sessions_for_scope`, `session_summary_scoped`, `session_observations_scoped`) | `crates/ai-memory-store/src/reader.rs` |
| Page-history readers (`page_versions`, `page_version_body`) and the bounded diff | `crates/ai-memory-store/src/reader.rs`, `crates/ai-memory-web/src/diff.rs` |
| Scoped search ranker shared with `memory_query` (`hybrid_search_scopes`) | `crates/ai-memory-web/src/recall.rs`, `crates/ai-memory-store/src/reader.rs` |
| Health panel checks and fix lines | `crates/ai-memory-web/src/health.rs`, `crates/ai-memory-core/src/fix_lines.rs` |
| Integration tests for API and web routes (auth, 400s, 404s, multi-scope correctness, SPA fallback) | `crates/ai-memory-web/tests/suite/routes.rs` |
| Auth + middleware layering | `crates/ai-memory-cli/src/commands/serve.rs` (`mount_web_router`, `apply_http_layers`) |
| Custom-UI dir validation | `crates/ai-memory-cli/src/commands/serve.rs` (`validate_web_ui_args`) |

## 9. CORS

`/api/v1` accepts cross-origin requests when the operator configures
the allow-list. The CORS layer is scoped to that router only; `/mcp`,
`/hook`, `/admin/*`, and `/web` stay same-origin.

Configure via either `--cors-allow-origin <origin>` (repeatable) on
the `serve` subcommand or `AI_MEMORY_CORS_ALLOW_ORIGINS=<csv>` in the
environment. The list is validated at startup:

- Each entry must be a fully-qualified `scheme://host[:port]` URL.
- No trailing slash, no path, no query, no wildcard (`*`).
- Mixed `http://` + `https://` is fine; pick what your SPA serves.

Invalid origins fail startup with a clear error rather than silently
accepting wildcard. The layer allows `GET / POST / OPTIONS`,
`Authorization` + `Content-Type` headers, and credentials, with a
10-minute preflight cache.

The CORS layer sits outside authentication. A browser sends its preflight
(`OPTIONS` with `Access-Control-Request-*`) without credentials, so the
preflight is answered by the allow-list alone. Every real request still needs a
bearer or session, and its `401`/`403` carries the CORS headers so the calling
page can read the refusal.

## 10. Known gaps and deliberate non-goals

- **No write surface, by design.** This is not a pending iteration: browsers
  can't mutate, and won't. The wiki is a record of what a project
  produced, authored by automated summarisation over captured
  observations; retrieval, provenance and the audit trail all rest on
  nobody having gone back and adjusted it. Hand-editing a page stops it
  answering "what did this project produce" and starts it answering
  "what did someone want it to say", with no way to tell the two apart
  afterwards.

  Human input is still allowed. `memory_write_page` exists for
  durable human annotations and lands them in the same lineage as
  everything else (pages supersede on body change). The line is between
  adding to the record through the normal path and editing it from
  outside. A human-editable wiki is a reasonable thing to want and a
  separate product; see #482. The history view shows what changed between
  versions; it has no restore button, for the same reason.
- **Rate limiting** is shared with `/mcp` + `/admin` (only the body
  cap is enforced today). A future global limiter would tighten the
  authenticated-misbehaviour case.

For status updates on any of these, the issue tracker is the source of
truth.

### Browser inbox exception

The `/api/v1` surface and wiki editing stay read-only. The built-in web UI adds
`/web/inbox` and `POST /web/messages` (under the configured base path and web
slug). The POST queues a sanitized, admitted message for an existing project and
harness. It requires write access to the source and destination, a JSON body,
the UI header and the normal browser-session CSRF checks. It does not mutate
wiki pages, pop messages or run an agent. See
[agent messaging](agent-messaging.md#in-the-web-ui).

Three additional POST routes support editing queued messages: `/web/messages/{id}/edit` holds it
out of the queue and returns its text, `/web/messages/{id}/save` replaces the
subject and text and requeues it, and `/web/messages/{id}/release` requeues it
unchanged. The body names the mailbox (`workspace`, `project`), and `save` adds
`subject` and `body`. They need the same checks as sending, plus write access to
the project the message came from. A message outside the expected state
returns 409.

## Bundled documentation for agents

The documentation catalog is at `GET /api/v1/docs?q=token`. Each entry has an
`id`, title, description, category and repository `source` path.
`GET /api/v1/docs/{id}` returns that metadata plus `markdown`, the binary's
package `version`, a selected `release` and the available `releases`. These are
read-only product guides, independent of project scopes and database content.
They use the same authentication as the other API routes. Search is limited to
200 characters, and unknown document ids and releases return 404.

The browser uses `/web/docs` and `/web/docs/{id}`. A `.md` suffix returns
`text/markdown` instead of HTML. The changelog defaults to Unreleased, and
archived series default to their first release. Use `?release=3.2.2` for one
release or `?release=all` for a whole series. The guides and changelog files are
compiled into the binary, including source-built Docker images, so no internet
connection or running LLM is needed. Repository links outside the curated
catalog open on GitHub. These pages describe shipped capabilities and do not
expose the running server's configuration or credentials. Apply your configured
base path and web slug to these examples.
