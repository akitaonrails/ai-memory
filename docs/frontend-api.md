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
| `/api/v1/*` | Browse workspaces, projects, pages; read full page markdown + frontmatter + back-links; FTS5 search (global or scoped, single or multi-project); aggregate "overview" snapshots; drill into stale / duplicate / orphan pages. | Write, delete, rename, lint, consolidate, run sweeps, manage handoffs. The `/api/v1` surface is **read-only by construction** — the handlers contain zero writer calls. Writes still go through `/admin/*` (used by the CLI) or MCP tools. |
| `--web-ui-dir` | Host any SPA at `/web` (or `--web-slug`), same-origin with the API, behind the same auth. The default built-in `/web` browser stays the fallback when the flag is absent. | Host the SPA on a *different* origin without a reverse proxy — use same-origin hosting or configure CORS deliberately (see §9). |

## 2. Auth model

Every `/api/v1/*` request goes through the same bearer + host-allowlist
middleware as `/mcp`, `/hook`, and `/admin/*` — they're all nested
*before* the auth layers are applied
(`crates/ai-memory-cli/src/commands/serve.rs`, see `mount_web_router` →
`apply_http_layers`). So:

- **Anonymous request → `401 Unauthorized`** (when the server is running
  with a bearer token configured).
- **Disallowed `Host` header → `403 Forbidden`** (DNS-rebinding guard).
- The same token protects everything; there is no per-user scoping.
  Single-tenant by design (see [`docs/design-decisions.md`](design-decisions.md) §13).

Pass the bearer in the standard header:

```http
Authorization: Bearer <token>
```

Get a token:

```bash
ai-memory generate-auth-token   # writes to stdout
# then export AI_MEMORY_AUTH_TOKEN=<token> in the server's environment,
# or put it under [auth].bearer_token in config.toml
```

In a same-origin SPA, the token can come from:

- A user-pasted value in the UI (the simplest model — same as the
  built-in `/web` browser's HTTP Basic prompt).
- A platform-specific secret store, then injected into `fetch()` calls.

> **XSS note:** if your SPA stores the bearer in `localStorage` and ships
> with an XSS bug, the token is exfiltrable. That's the SPA's risk, not
> the API's. Consider read-only environment-injected tokens or HTTP-only
> cookie tunneling via a reverse proxy if you're hardening for that.

## 3. Error model

All errors return a JSON body of shape:

```json
{ "error": "human-readable message" }
```

with one of these statuses:

| Status | When |
|---|---|
| `400 Bad Request` | invalid query params, malformed `Authorization`, partial scope (workspace without project or vice versa), too many scopes in `POST /search` (>25), empty `q`. |
| `401 Unauthorized` | bearer missing or wrong. |
| `403 Forbidden` | Host header not in allowlist. |
| `404 Not Found` | workspace, project, or page doesn't exist; or page file missing on disk. |
| `500 Internal Server Error` | reader pool / SQLite failure. Body is `{"error":"<context>"}` with the source error chain. |

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
```

### Handoff listing

`state` accepts `open` | `accepted` | `expired`; omit it to list every state,
which is how you find a baton that was already consumed. Results are scoped by
owner: an authenticated caller sees their own plus the shared handoffs, an
anonymous browser sees only shared ones — an owned handoff (and the prompt-
derived text inside it) is never rendered to someone it does not belong to.
The same scoping applies to the `handoff` field of both overview endpoints and
to `pending_handoff_count`, so the count and the fetch always agree.

On a server that authenticates, the listing's prompt-derived fields —
`summary`, `open_questions`, `next_steps` — are served to a caller the server
can name and to the root operator; an automatic handoff synthesises them
verbatim from the operator's prompts, and the listing returns the project's
whole history rather than the single newest open row. A caller that is neither
named nor root gets the fields absent and `redacted` set to `true`; the metadata
(state, timestamps, agent, cwd, touched files, ownership) is always served. A
server with no auth configured serves the bodies, since it already serves every
page body unauthenticated.

"Can name" means the identity the auth tier itself resolved: the username when
one was forwarded, otherwise the asserted subject claim. An ingress that
terminates OIDC and forwards only `X-Memory-Actor-Sub` therefore reads its own
handoffs and the shared ones with `redacted: false`, and no rung of the auth
chain produces an authenticated-but-unnameable caller today — the redacting arm
is a fail-safe floor, not a live tier.

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
      "owner": "alice",
      "accepted_by": "alice",
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
**exempt from bearer auth and host allowlist**: a browser opening a
fresh tab gets the icon without an HTTP Basic prompt, and the
embedded PNG is the same one any visitor to `/web` already sees, so
the info-leak surface is nil.

## 5. Limits and pagination

- All `limit` query params clamp to `1..=100`.
- `POST /api/v1/search`: at most **25 scopes** per request.
- HTTP body cap: **10 MB** (shared with the MCP body limit; you won't
  hit this for normal API traffic).
- **Cache-Control + ETag.** Idempotent read endpoints (workspaces,
  projects, pages list, page read, recent, briefing, overview) send
  `Cache-Control: private, max-age=N` with an N tuned per endpoint
  and a SHA-256 `ETag` derived from the response body. Browsers that
  echo back `If-None-Match` receive a `304 Not Modified` with no body.
  Search responses are not cacheable (request body affects the result).

## 6. Custom UI hosting and base paths

```bash
ai-memory serve \
    --transport http \
    --bind 127.0.0.1:49374 \
    --enable-web \
    --web-ui-dir /path/to/your-spa/dist
```

The static directory is served at `/web` via `tower-http::ServeDir`:

- **Same auth as `/api/v1`.** Mounted before the bearer middleware
  layer, so `/web/*` requests must carry the same `Authorization`
  header (browsers typically prompt via HTTP Basic when auth is on —
  the user pastes the token as the password).
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
// Resolve the API base from the SPA shell injected by ai-memory. The meta tag
// is empty at host root and e.g. "/wiki" behind a subpath reverse proxy.
const basePath = document
  .querySelector('meta[name="ai-memory-base-path"]')
  ?.getAttribute("content") ?? "";
const API = `${location.origin}${basePath}/api/v1`;
const TOKEN = localStorage.getItem("ai-memory-token"); // your storage choice

async function apiGet(path, params) {
  const url = new URL(`${API}${path}`, location.origin);
  if (params) Object.entries(params).forEach(([k, v]) =>
    v != null && url.searchParams.set(k, v));
  const resp = await fetch(url, {
    headers: {
      Accept: "application/json",
      ...(TOKEN ? { Authorization: `Bearer ${TOKEN}` } : {}),
    },
  });
  if (!resp.ok) {
    const { error } = await resp.json().catch(() => ({ error: resp.statusText }));
    throw new Error(`${resp.status}: ${error}`);
  }
  return resp.json();
}

// Top-level "home" view in one request.
const overview = await apiGet("/workspaces/default/projects/ai-memory/overview", { limit: 10 });
console.log(overview.briefing.counts.pages_latest, "pages");

// Multi-scope search.
const search = await fetch(`${API}/search`, {
  method: "POST",
  headers: {
    "Content-Type": "application/json",
    Authorization: `Bearer ${TOKEN}`,
  },
  body: JSON.stringify({
    q: "karpathy",
    scopes: [
      { workspace: "default", project: "ai-memory" },
      { workspace: "default", project: "shared-notes" },
    ],
    limit: 20,
  }),
}).then(r => r.json());
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
| Response structs (`PageHit`, `WorkspaceSummary`, `BriefingSnapshot`, `HealthPage`, …) | `crates/ai-memory-store/src/reader.rs` |
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

## 10. Known gaps (planned iterations, not blockers)

- **Write surface.** Browsers can't mutate today (notes, consolidate,
  lint, purge — all live under `/admin/*` for the CLI or under MCP
  tools for agents). A thin authenticated write surface ("edit this
  page" from the browser) is a deliberate v2 conversation.
- **Rate limiting** is shared with `/mcp` + `/admin` (only the body
  cap is enforced today). A future global limiter would tighten the
  authenticated-misbehaviour case.

For status updates on any of these, the issue tracker is the source of
truth.
