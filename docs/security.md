# Security

> The full security model. Moved verbatim from the README front page; the README keeps a summary.

Loopback-only (`127.0.0.1:49374`) with no auth is the default because
it is safe for a single-user laptop: no process outside the machine can
reach the server.

Unauthenticated non-loopback HTTP now fails closed. Set
`AI_MEMORY_AUTH_TOKEN` or bind loopback; `--allow-insecure-no-auth` is an
intentional, dangerous exception for plain HTTP only. Authentication does not
encrypt bearer tokens: for LAN or remote access, use the ready
[Caddy](../docker/compose.tls.caddy.yml) or
[Cloudflare Tunnel](../docker/compose.tls.cloudflared.yml) templates described in
the [HTTPS reverse-proxy guide](https-via-proxy.md).

Enable bearer auth when the server is exposed beyond loopback, when
untrusted local processes share the machine, or when the data dir holds
sensitive project history:

```bash
TOKEN=$(ai-memory generate-auth-token)

docker run -d --name ai-memory \
    --restart unless-stopped \
    -p 0.0.0.0:49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_AUTH_TOKEN="$TOKEN" \
    -e AI_MEMORY_ALLOWED_HOSTS="<server-ip>,localhost,127.0.0.1" \
    akitaonrails/ai-memory:latest

ai-memory install-mcp   --client claude-code --apply \
    --server-url "http://<server-ip>:49374/mcp" --auth-token "$TOKEN"
ai-memory install-hooks --agent  claude-code --apply \
    --server-url "http://<server-ip>:49374" --auth-token "$TOKEN"
```

Bearer auth protects `/mcp`, `/hook`, `/handoff`, `/workstream/*`, and
machine calls to `/admin/*` and `/api/v1/*`. Humans sign in at
`POST /auth/login`; the console uses an `HttpOnly` session cookie plus CSRF,
not a Bearer in `localStorage`. Custom SPA HTML at `/web` is public static.
When human auth listens beyond loopback,
`AI_MEMORY_AUTH__SECURE_COOKIE=true` is required and signals that a trusted
HTTPS reverse proxy owns the browser-facing edge. It makes the session cookie
HTTPS-only. Close or redirect direct HTTP access to that hostname.
Non-loopback binds should also set `AI_MEMORY_ALLOWED_HOSTS` to guard against
DNS rebinding.

Busy shared hook servers can also set `AI_MEMORY_HOOK_RATE_PER_SEC` (tokens per
second per actor/session source) and optionally `AI_MEMORY_HOOK_RATE_BURST` to
bound one runaway session without blocking unrelated hook sources. Unset or `0`
rate leaves the limiter disabled.

For shared servers where each developer should authenticate their own hook
writes, native Claude Code hooks can use a stored OIDC device token instead of
embedding a shared static token:

```bash
ai-memory auth login oidc-device \
    --issuer "https://issuer.example.com/realms/team" \
    --client-id "ai-memory-cli"

ai-memory install-hooks --agent claude-code --apply \
    --server-url "http://<server-ip>:49374"
```

OIDC hook auth requires the native `ai-memory hook ...` command path. The Docker
wrapper keeps shell-script hooks by default; set up OIDC from a native release
binary or source install. Thin-client HTTP commands such as `ai-memory status`
and `ai-memory search` also use the stored OIDC access token when no static
`AI_MEMORY_AUTH_TOKEN` / `[auth].bearer_token` is configured; the static bearer
still wins when present. This is for OIDC-aware gateways/bridges; native
ai-memory server auth still accepts static root bearer / DB-user tokens, and
`/admin/*` remains root-only unless a gateway translates accepted OIDC auth into
upstream auth that ai-memory accepts.

OIDC/Keycloak session ids are login-provider sessions, not ai-memory agent
sessions. Shared servers that rely on `[auto_scope]` session isolation still
need explicit `workspace` + `project` / `scopes`, or a bridge that forwards the
real lifecycle-hook session id on MCP requests.

**Want HTTPS?** ai-memory deliberately does not terminate TLS itself —
the right answer is a battle-tested reverse proxy in front of it.
[`docs/https-via-proxy.md`](https-via-proxy.md) is the deployment
guide, with copy-paste docker compose templates in
[`docker/compose.tls.caddy.yml`](../docker/compose.tls.caddy.yml) (Caddy
with Let's Encrypt or internal CA) and
[`docker/compose.tls.cloudflared.yml`](../docker/compose.tls.cloudflared.yml)
(Cloudflare Tunnel — no open ports). Both are recommended once you
turn on multi-user or bind beyond loopback. The Quick Start happy
path of single-user on loopback doesn't need TLS — that case is
called out explicitly in the guide so you don't add ceremony where
it doesn't earn its keep.

**Multi-user attribution (v0.8, optional) plus human login.** When more
than one human shares a server, ai-memory attributes each write to a
named user. Humans sign in with username/password; agents and CLIs use
`Authorization: Bearer` (`AI_MEMORY_AUTH_TOKEN` for root automation, or
an `aim_` key from `ai-memory api-key add`). Data stays single-tenant —
there is no per-page RBAC. A
`[auth].token_pepper` is required for DB-user authentication, but creating the
first user row is what immediately switches every `/admin/*` endpoint to
root-only, including status/search/read-page and user-management routes.
`ai-memory init` generates a pepper for new installs without changing
single-user behavior until a user is added. An SSO gateway can instead use a
dedicated `[auth].actor_proxy_bearer_token` and trusted `X-Memory-Actor-*`
headers; its credential is deliberately separate from the root bearer so a
missing identity cannot become root. See
[`docs/users.md`](users.md) for the full walkthrough and the
four-rung auth ladder.

See [`docs/deploy.md`](deploy.md) for the full homelab pattern
with bearer auth, host allowlisting, and TLS/reverse-proxy options.
