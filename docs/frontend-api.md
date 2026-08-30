# Frontend integration: `/api/v1`

> Read-only JSON API and custom-UI hosting model for building third-party
> frontends against an `ai-memory` server. Added in **v0.6.0** (PR #7).
> Everything below is sourced from the actual route handlers in
> `crates/ai-memory-web/src/routes/api.rs` and the response structs in
> `crates/ai-memory-store/src/reader.rs` — keep them as the canonical
> reference if anything here drifts.

## 1. What this surface is (and isn't)

| | What you can do | What you can't do |
|---|---|---|
| `/api/v1/*` | Browse workspaces, projects, pages; read full page markdown + frontmatter + back-links; FTS5 search (global or scoped, single or multi-project); aggregate "overview" snapshots; drill into stale / duplicate / orphan pages; list a project's sessions and read one session's raw observations. | Write, delete, rename, lint, consolidate, run sweeps, manage handoffs. The `/api/v1` surface is **read-only by construction** — the handlers contain zero writer calls. Writes still go through `/admin/*` (used by the CLI) or MCP tools. |
| `--web-ui-dir` | Host any SPA at `/web` (or `--web-slug`), same-origin with the API, behind the same auth. The default built-in `/web` browser stays the fallback when the flag is absent. | Host the SPA on a *different* origin without a reverse proxy — use same-origin hosting or configure CORS deliberately (see §9). |

## 2. Auth model

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
- `/mcp`, hooks, handoffs, and workstream routes are machine-only. A web-session
  cookie cannot authenticate them.
- A disallowed `Host` header receives `403 Forbidden` before auth evaluation
  (DNS-rebinding guard).

The custom SPA shell and its static assets are public so the login screen can
load. Its data routes remain protected. Start with:

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

Do not put `AI_MEMORY_AUTH_TOKEN`, `aim_` keys, session values, or CSRF values
in `localStorage`. Before any human password or completed bootstrap exists,
deprecated GET-only browser compatibility may accept the root bearer through
HTTP Basic and an HttpOnly `ai_memory_auth` cookie. Human activation disables
that path immediately. See [`docs/users.md`](users.md) for bootstrap, password
rotation, recovery, roles, session expiry, and API-key lifecycle.

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

**Response:** `{ "workspaces": [WorkspaceSummary, …] }`

```json
{
  "workspaces": [
    {
      "workspace_name": "default",
      "project_count": 3,
      "page_count": 412,
      "last_updated": "2026-05-28T14:02:11.123Z"
    }
  ]
}
```

`last_updated` is `null` for an empty workspace.

### 4.2 Projects

```http
GET /api/v1/projects                  # all projects across all workspaces
GET /api/v1/projects?workspace=NAME   # projects in one workspace
```

**Response:** `{ "projects": [ProjectSummary, …] }`

```json
{
  "projects": [
    {
      "workspace_name": "default",
      "project_name": "ai-memory",
      "page_count": 138,
      "last_updated": "2026-05-28T14:02:11.123Z"
    }
  ]
}
```

### 4.3 Pages (list)

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/pages
```

**Response:** `{ "pages": [PageSummary, …] }`

```json
{
  "pages": [
    {
      "path": "decisions/0007-db.md",
      "title": "Standardised on Postgres",
      "kind": "decision",
      "tier": "semantic",
      "updated_at": "2026-05-27T09:12:00.000Z"
    }
  ]
}
```

`404` if the workspace or project doesn't exist.

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
  "body": "# Standardised on Postgres\n\n…",
  "links":     [ { "path": "concepts/db-rules.md", "title": "DB rules", "kind": "rule" } ],
  "backlinks": [ { "path": "sessions/2026-05-27.md", "title": "Session 2026-05-27", "kind": "session" } ]
}
```

`404` for missing workspace/project, missing page row, or missing file
on disk (the body is read from the markdown file at request time).

### 4.5 Search

Two forms — query-string for the common single-scope or global case,
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

**Response:** `{ "hits": [PageHit, …] }`

```json
{
  "hits": [
    {
      "id": "01928d27-…",
      "path": "concepts/karpathy-wiki.md",
      "title": "Karpathy LLM Wiki pattern",
      "snippet": "Andrej <mark>Karpathy</mark>'s LLM wiki design …",
      "rank": -8.4
    }
  ]
}
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
- `rank` is FTS5 rank — **lower is better** (closer to query terms).

### 4.6 Recent

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/recent?limit=20
```

`is_latest = 1` pages ordered by `updated_at` DESC. `limit` clamped
`1..=100`, default `10`.

Every reader surface uses the same `kind` contract. An explicit frontmatter
`kind` wins; otherwise the path families `_rules/`, `_slots/`, `sessions/`,
`decisions/`, `gotchas/`, `concepts/`, `procedures/`, and `notes/` derive
`rule`, `slot`, `session`, `decision`, `gotcha`, `concept`, `procedure`, and
`note`, respectively. Other paths fall back to `fact`.

**Response:** `{ "pages": [BriefingPage, …] }`

```json
{
  "pages": [
    {
      "path": "sessions/2026-05-28.md",
      "title": "Session 2026-05-28",
      "kind": "session",
      "updated_at": "2026-05-28T14:02:11.123Z"
    }
  ]
}
```

### 4.7 Briefing (structured snapshot)

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/briefing?limit=10
```

Same payload `memory_briefing` returns — counts + activity windows +
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
anonymous browser sees only shared ones — an owned handoff (and the prompt-
derived text inside it) is never rendered to someone it does not belong to.
The same scoping applies to the `handoff` field of both overview endpoints and
to `pending_handoff_count`, so the count and the fetch always agree.
For recovery, a root-authorized request may pass `all_owners=true` to list all
operators' rows. User and anonymous requests receive `403`; the default remains
own plus shared, including for root.

On a server that authenticates, the listing's prompt-derived fields —
`summary`, `open_questions`, `next_steps` — are served to a caller the server
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
authenticated-but-unnameable caller today — the redacting arm is a fail-safe
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

> Note: `handoff` is **not** consumed by the read API — the
> handoff stays "open" and can still be accepted by the next agent.

### 4.9 Cross-project graph

```http
GET /api/v1/graph
```

Returns every resolved wikilink whose endpoints sit in different
projects, with both endpoints' workspace + project + path. Useful for
rendering a project-level dependency view in the SPA.

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

Global today (no workspace / project filter); narrower query params
are a follow-up.

### 4.10 Browser tab icon

```http
GET /favicon.ico
```

Returns the same transparent PNG the built-in web UI serves as the
header logo. Browsers fetch this path automatically. The route is
present whenever the web UI is enabled (`--enable-web`) and is
mounted at the absolute host root — outside `--base-path` and outside
the `/web` nest — so the browser's automatic fetch reaches it even
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
`false` (only sessions with `ended_at` set). Owner-filtered like handoffs: a
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
      "actor_user": null
    }
  ]
}
```

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
    "actor_user": null
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

## 5. Limits and pagination

- Most `limit` query params clamp to `1..=100`; handoff history and
  session observations clamp to `1..=200`. Session listing and session
  observations take an `offset`; observations also report `total`.
- Session observation bodies are capped per row by `body_max_chars`
  (`200..=16384`, default `4000`) with a visible truncation marker.
- `POST /api/v1/search`: at most **25 scopes** per request.
- HTTP body cap: **10 MB** (shared with the MCP body limit; you won't
  hit this for normal API traffic).
- **Cache-Control + ETag.** Identity-independent read endpoints use
  `Cache-Control: private, max-age=N` with an endpoint-specific TTL; page reads
  also carry a SHA-256 `ETag`, and a matching `If-None-Match` receives `304 Not
  Modified`. Briefing, overview, handoff-list, session-list and session
  observation responses depend on the authenticated actor and therefore use
  `Cache-Control: private, no-store`, so
  a browser cannot reuse Alice's prompt-derived response after credentials at
  the same URL switch to Bob. Search responses are not cacheable because the
  request body affects the result.

## 6. Custom UI hosting and base paths

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
  but at the segment boundary they mean "current" and "parent" — one
  typo and your prefix is a traversal vector.
- Any character outside the unreserved set (spaces, `<`, `"`, etc.).
- Empty / whitespace-only input.

The trailing-slash redirect at `{base_path}{web_slug}/` →
`{base_path}{web_slug}` keeps the query string. Fragments are
client-side and never reach the server.

When `--web-ui-dir` is **absent**, the built-in server-side `/web`
browser is the default (read-only HTML rendering, FTS5 search,
project tree). No regression.

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
TOKEN=$(ai-memory generate-auth-token)
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
| 27 integration tests covering every endpoint (auth, 400s, 404s, multi-scope correctness, SPA fallback) | `crates/ai-memory-web/tests/routes.rs` |
| Auth + middleware layering | `crates/ai-memory-cli/src/commands/serve.rs` (`mount_web_router`, `apply_http_layers`) |
| Custom-UI dir validation | `crates/ai-memory-cli/src/commands/serve.rs` (`validate_web_ui_args`) |

## 9. CORS

`/api/v1` accepts cross-origin requests when the operator configures
the allow-list. The CORS layer is scoped to that router only — `/mcp`,
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

## 10. Known gaps and deliberate non-goals

- **No write surface, by design — not a pending iteration.** Browsers
  can't mutate, and won't. The wiki is a record of what a project
  produced, authored by automated summarisation over captured
  observations; retrieval, provenance and the audit trail all rest on
  nobody having gone back and adjusted it. Hand-editing a page stops it
  answering "what did this project produce" and starts it answering
  "what did someone want it to say", with no way to tell the two apart
  afterwards.

  This is not a ban on human input. `memory_write_page` exists for
  durable human annotations and lands them in the same lineage as
  everything else (pages supersede on body change). The line is between
  adding to the record through the normal path and editing it from
  outside. A human-editable wiki is a reasonable thing to want and a
  separate product — see #482.
- **Rate limiting** is shared with `/mcp` + `/admin` (only the body
  cap is enforced today). A future global limiter would tighten the
  authenticated-misbehaviour case.

For status updates on any of these, the issue tracker is the source of
truth.
