# Security Policy

## Reporting a vulnerability

Please **do not open a public GitHub issue** for security vulnerabilities.

Report security issues by opening a [private security advisory](https://github.com/akitaonrails/ai-memory/security/advisories/new)
on GitHub. You will receive a response within 7 days. If the issue is confirmed
we will aim to release a patch within 30 days and credit you in the changelog
(unless you prefer to remain anonymous).

If you cannot use GitHub's private advisory flow (e.g. your organization's
vendor-security process requires a named point of contact independent of a
GitHub account), open an issue asking for an alternate contact channel and a
maintainer will follow up; do not put vulnerability details in that issue.

## Threat model

ai-memory is a **single-tenant workstation/homelab service**. It supports
multiple attributed users. Projects are `open` by default, so every
authenticated user can read the same project memory; a root operator can
mark a project `restricted` so only root, its creator, and granted database
users reach it (see "Per-project isolation" below). The following describes
what the project is and is not designed to defend against.

### In scope

- **Local data confidentiality.** Wiki files and the SQLite database live
  under a single data directory controlled by the operating-system user who
  runs the server. We rely on filesystem permissions; no additional
  encryption at rest is provided in v1. On Unix, newly created data
  directories are owner-only (`0700`) and newly created configuration,
  SQLite, managed-workstream segment, and downloaded backup files are owner
  read/write only (`0600`), independent of umask. Existing installations are
  not chmodded automatically. Windows uses its filesystem ACLs rather than
  POSIX mode bits.

  The hook spool (`<data_dir>/hook-spool/`) holds each captured lifecycle
  event until the server accepts it. Each file carries the event body as the
  harness sent it, before the server's privacy strip runs (the client scrubs
  only a field that exceeds its size cap and the opt-in assistant excerpt),
  plus a copy of the static hook bearer when the event authenticates with one;
  an OIDC event stores no token and resolves it at delivery. Files are `0600`
  in a `0700` directory. A drain at session boundaries delivers and deletes
  them; an event that cannot be delivered is dropped after 7 days, and the
  spool is capped at 10,000 files. `ai-memory backup` does not include the
  spool, but a filesystem-level copy of the data directory does.

  An `ai-memory backup` archive includes `<data_dir>/config.toml` verbatim,
  which may hold the API-key pepper and other inline credentials; protect
  the archive as a secret (see
  [`docs/lifecycle-ops.md`](docs/lifecycle-ops.md#backup)). `restore` keeps
  an existing live config unless `--with-config` is passed.

- **Network exposure when binding to non-loopback addresses.** If you run
  `ai-memory serve --bind 0.0.0.0:…` you are exposing the MCP and admin
  routes to your local network. Protect this with:
  - `AI_MEMORY_AUTH_TOKEN` / `ai-memory generate-auth-token` (a bearer
    token checked on every authenticated surface). `/healthz`, `/auth/login`,
    `/auth/recovery`, `/favicon.ico`, and the custom `/web` static assets are
    public by design; the builtin wiki browser and `/api/v1` stay behind
    auth.
  - Firewall rules or a reverse proxy with TLS.

  The server fails closed before serving unauthenticated non-loopback HTTP.
  `--allow-insecure-no-auth` is a deliberate dangerous override for an
  intentional plain-HTTP LAN deployment.

  **Inside a container this check warns instead of refusing.** Publishing a
  port with `-p` requires binding `0.0.0.0` in the namespace, so the bind
  address says nothing about reachability there. Reachability is decided by the
  host-side publish spec, which the process cannot see. In a container the
  thing to check is your own `-p`: `-p 127.0.0.1:49374:49374` is loopback-only
  and safe without a token; anything broader needs `AI_MEMORY_AUTH_TOKEN`.
  The `Host` allowlist is not a substitute: it defends against DNS
  rebinding, where a browser sets the header, and a client that can route to
  the port sets `Host` freely. Authentication does not encrypt
  bearer tokens, so use a TLS reverse proxy for traffic beyond loopback; see
  [`docs/https-via-proxy.md`](docs/https-via-proxy.md).

  For `/web` behind that proxy, set `AI_MEMORY_AUTH__SECURE_COOKIE=true` to
  make its browser session cookie HTTPS-only. ai-memory does not trust
  forwarded-protocol headers to decide this. Close or redirect direct HTTP
  access; browsers intentionally withhold Secure cookies over HTTP.

- **Host-header DNS rebinding.** The HTTP server enforces an
  `AI_MEMORY_ALLOWED_HOSTS` allowlist (defaulting to `localhost`, `127.0.0.1`,
  and `::1`). Requests with a `Host` header not in the list are rejected
  with 403.

- **Request body size.** Inbound HTTP bodies are capped to prevent trivial
  memory exhaustion: 10 MB on `/mcp`, `/hook`, and `/workstream/*`, and
  32 MB on the `/admin/*` routes, whose `/admin/bootstrap` can carry a large
  JSON array of sources. The remaining routes (web, `/api/v1`, `/auth/*`)
  use axum's 2 MB default.

- **Authentication and administrative authorization.** The documented auth
  ladder (`docs/users.md`) is: anonymous when no root bearer is configured,
  the static root bearer, a proxy-asserted user (a distinct
  `[auth].actor_proxy_bearer_token` plus trusted `X-Memory-Actor-*` headers,
  which is root only on an exact `root_issuer` + `root_subject` match), and
  database-user `aim_` tokens, which attribute writes but never grant admin
  access; an unknown bearer gets 401. The server itself does not validate
  OIDC tokens. `ai-memory auth login oidc-device` stores a per-developer
  token for an external OIDC-aware gateway or bridge in front of the server,
  which must translate it into one of the rungs above (`docs/sso.md`). The
  first database user (or a configured proxy bearer) makes every `/admin/*`
  route root-only.

- **Per-project isolation.** Wiki files and SQLite rows are namespaced by
  `(workspace_id, project_id)`. A purge operation for project A cannot
  delete files that also belong to project B. Entity lookup filters at the
  project CTE and page boundaries; V38 triggers reject mismatched
  workspace/project entities and cross-project entity/page links. The wiki
  directory is imported, operator-editable content and is therefore treated as
  untrusted: symbolic links and filesystem reparse points at the wiki root,
  workspace/project roots, or any descendant are refused for page reads,
  writes, indexing, recovery, and lifecycle cleanup. These component checks
  protect against links already present when an operation runs; they do not
  claim descriptor-relative protection against a concurrently malicious process
  running as the same operating-system user.

  On a multi-user server, per-project access modes gate entry on top of that
  boundary: every project is `open` until an operator restricts it
  (`ai-memory project access --mode restricted`), after which only root, the
  project's creator, and users holding a grant (`ai-memory user grant`) may
  reach it, and refusals are explicit 403s rather than empty results. Grants
  apply to database users. Root and single-operator installs are never
  gated, and a user asserted by the trusted-proxy bearer is not subject to
  per-project grants: that identity maps to no database `UserId` to key a
  grant on, so in a trusted-proxy deployment the proxy itself is the
  authorization boundary (`docs/users.md`).

- **Entity text remains bounded local data.** Consolidator output and
  hand-edited `entities:` frontmatter cross the same normalization boundary:
  at most 10 names per page, 64 characters per name, with control characters
  rejected. Query tokens and SQL parameters are bounded, and lexical entity
  matching does not add an outbound provider call. Entity names remain
  untrusted stored content when rendered in an explain response.

- **Assistant/Stop capture is opt-in and sanitized (#196).** The assistant's
  final turn is never persisted by default. Storing it requires a **double
  opt-in**: `capture_assistant` on the server and `install-hooks
  --capture-assistant` on the client. When enabled, be aware that:
  - The excerpt is sanitized twice. The client scrubs with the built-in
    patterns *before* it reaches the spool or wire, and the server re-scrubs
    with its configured `[sanitize]` patterns before storing. Operator
    `extra_patterns` run only on the server side, so a secret matched only by an
    `extra_patterns` rule may still sit in the excerpt on the client spool/wire
    before it reaches the server. Client-side redactions are irreversible: the
    server's `allowlist` cannot restore text the client already replaced with
    `[REDACTED]`.
  - Captured assistant text flows into the consolidation and reviewer prompts,
    and, if you configure a cloud LLM provider, is sent to that provider.
    The latest excerpt of a session also rides in its automatic handoff, so
    the next session that claims the baton receives it as startup context. It
    is not rendered into the git-tracked session page.
  - The opt-in is **global** to the install: there is no per-project marker to
    exclude a sensitive repository once the flag is on (assistant text is not
    path-attributable). Turn the server flag off to disable it everywhere.
  - The excerpt can quote code, secrets, or content from paths ai-memory never
    sees; the `Sanitizer` is a best-effort credential strip, not a guarantee
    (see the injection note below).

- **Stored-content prompt injection.** Handoffs, project briefs, managed
  workstream packets, MCP routing, and LLM maintenance prompts explicitly mark
  stored material as untrusted historical data. Sanitization and structured
  output schemas remain defense in depth, not proof that a model cannot be
  manipulated; operators and agents must verify security-sensitive claims
  against current instructions and the checkout.

- **Search reranking is an outbound-data opt-in.** Setting
  `AI_MEMORY_RERANKER=llm` sends each eligible live query plus bounded page
  titles and search snippets to the configured LLM provider.
  `AI_MEMORY_RERANKER=decisions-choice` sends those values directly to OpenAI Decisions, together with the candidate page-version IDs, which are time-ordered UUIDs that reveal each version's creation time.
  Its API key goes only to `https://api.openai.com:443`; an HTTP loopback IP override never receives Authorization, and all redirects are rejected.
  The client ignores inherited proxy settings so a proxy cannot forward a loopback request off-host.
  Managed writes use
  ai-memory's sanitizer, but manually edited wiki files can contain unsanitized
  text; the live query is bounded but is not sanitized because redaction could
  change its meaning. JSON encoding, an explicit untrusted-data prompt, strict
  score validation, a timeout, and a four-call concurrency cap limit control and
  availability impact, but they do not make a cloud provider private. Leave
  reranking off or use a local provider when queries or recalled snippets must
  not leave the server.

- **Published executable integrity.** Docker wrapper and standalone hook
  installs use GitHub Release assets with SHA-256 companions. GitHub Actions
  are pinned to reviewed commits and release jobs default to read-only token
  permissions except the GitHub Release publisher. Gitleaks checks each pushed
  or proposed commit range and a separate weekly/manual workflow checks the
  complete reachable history. The full-history scan recognizes only reviewed,
  exact fingerprints in `.gitleaksignore`; adding an entry never substitutes
  for removing the value from the current tree and rotating a real credential.

### Out of scope for v1

- **Private memory and per-page ACLs.** Database users provide attribution,
  not private memory. Per-project access modes and grants decide which
  projects a database user may reach, but inside a project every reader sees
  every page: there is no per-page RBAC, no private per-user pages, and no
  per-project restriction applied to proxy-asserted identities. Run separate
  servers/data directories for users who must not see one another's data.
- **Encryption at rest.** The data directory is a plain filesystem tree.
- **Remote sync security.** If you push the wiki git repository to a remote,
  securing that channel is your responsibility (SSH keys, GitHub access
  controls, etc.).
- **Perfect semantic prompt-injection prevention.** The privacy strip removes
  obvious credentials and the prompt surfaces preserve a trust boundary, but
  no text filter can prove that an LLM will ignore every adversarial passage.
- **Hostile-Internet denial of service.** Hook queues, request bodies, rate
  limits, and concurrency are bounded, but the service is not designed for
  direct untrusted-Internet exposure. Put it behind normal network controls.

## Supported versions

Only the latest release receives security fixes. We do not backport to older
minor versions.
