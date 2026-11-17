# Security

> The full security model. Moved from the README front page; the README keeps a summary.

A new install requires a bearer token. `ai-memory init` on a fresh data
dir writes a random root bearer to `<data_dir>/auth-tokens/default` (owner
only) and records `[auth].bearer_token_file`; the server then refuses every
route except `/healthz` without it. The hooks, the CLI and the
`ai-memory mcp-bridge` stdio bridge read it from the data dir, so the token is
never pasted into a harness config or passed on a command line. Claude Code
and Meta Muse Code run the session-aware bridge, Codex, Prime Agent and Factory Droid reference
the token as `$AI_MEMORY_AUTH_TOKEN`, and every other MCP client `install-mcp` supports
runs `ai-memory mcp-bridge --no-session-id` as a local stdio server. ZCode,
which takes only HTTP entries, is the exception and needs a manual entry.

An install created before this default keeps its loopback-only
(`127.0.0.1:49374`), no-auth behaviour. That stays safe from the network, but
every local process and browser page can reach the server. `ai-memory status`
and `ai-memory doctor` show `auth: none (loopback)` with the fix,
`ai-memory auth enable`, which mints the token, records it in config, gives
the hooks the token and re-registers every MCP client that points at this
server. Restart
the server afterwards.

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

When users must not see each other's projects at all, run separate servers.
One machine can still capture for several of them: register each server as
a named profile with `ai-memory server add` and let each repository's
`.ai-memory.toml` select one with `server = "<name>"`. The marker never holds
a URL or token, a profile can be restricted to the directories it serves, and
a selection that does not resolve drops the event instead of sending it to
another server. See
[`marker-file.md`](marker-file.md#routing-capture-to-another-server-server).

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

For a private LAN where the operator deliberately accepts plaintext transport,
`serve --allow-insecure-human-auth` (environment
`AI_MEMORY_ALLOW_INSECURE_HUMAN_AUTH=true`) permits a non-loopback listener
with a Non-Secure human session cookie. This exception is disabled by default
and prints an unconditional startup warning. It does not weaken password
checks, session or API-key authorization, CSRF, rate limits, the Host allowlist,
or the separate unauthenticated-exposure guard. It also provides no encryption:
any peer able to observe or alter LAN traffic can steal passwords, Bearer/API
credentials, session cookies, and memory data. Prefer the HTTPS proxy path.
The option changes no stored user, password, session, or API-key state; removing
it restores the startup refusal.

For the official Docker image, the environment form avoids replacing its
default `serve` command:

```bash
docker run -d --name ai-memory \
    -p 192.168.1.100:49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_ALLOW_INSECURE_HUMAN_AUTH=true \
    -e AI_MEMORY_ALLOWED_HOSTS="192.168.1.100,localhost,127.0.0.1" \
    akitaonrails/ai-memory:latest
```

With no token (the loopback default), a web page open in your browser can
still address `127.0.0.1:49374`. The server refuses those requests on the
machine and admin routes: a request the browser marks as cross-site
(`Sec-Fetch-Site`), or one whose `Origin` is not the server's own host, gets a
403. So does any browser request that is not a script `fetch()` call
(`Sec-Fetch-Dest` other than `empty`): a link, image or navigation is refused
even when it comes from the server's own pages or from another application.
Hooks, the CLI and MCP clients never send those headers, and the web UI's own
admin calls use `fetch()`.

Browsers send that fetch metadata over plain HTTP only to `localhost`,
`*.localhost`, `127.0.0.0/8` and `[::1]`. When an unauthenticated server is
reached through any other name in the `Host` allowlist (a LAN address or name
with `--allow-insecure-no-auth`, or `host.docker.internal` in the Docker
image's default list), an image request carries neither fetch metadata nor
`Origin`. There the session-start handoff fetch, the only state-changing `GET`,
must also carry the `X-AI-Memory-Client` header that every ai-memory hook,
CLI command and generated integration sends; a web page cannot add it without
a CORS preflight, which these routes never answer. Hooks staged before this
header existed get a 403 on such a server until you re-run
`ai-memory install-hooks` (or `ai-memory upgrade`); on a loopback host they
keep working unchanged. A request that carries a proxy's forwarding header
(`Forwarded`, `X-Forwarded-For`, `X-Forwarded-Host`, `X-Forwarded-Proto`,
`X-Forwarded-Server`, `X-Forwarded-Port`, `X-Original-Forwarded-For`,
`X-Real-IP`, `X-Client-IP`, `CF-Connecting-IP`, `True-Client-IP` or `Via`)
counts as non-loopback here even when its `Host` is a loopback address,
because a plain-HTTP reverse proxy may rewrite `Host` to the address it
forwards to while the browser behind it saw a non-loopback origin. ai-memory's
own clients connect directly and send none of these headers. The server cannot
see a proxy that says nothing, though: nginx's default `proxy_pass` rewrites
`Host` to the upstream address and adds no `Forwarded`, `X-Forwarded-*` or
`Via` header, so its requests look like a local client's. A plain-HTTP proxy in
front of an unauthenticated server must therefore add `X-Forwarded-For`
(`proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;`) or keep the
browser's host (`proxy_set_header Host $host;`, which also needs that name in
the `Host` allowlist). Better still, terminate TLS at the proxy or set a bearer
token. A browser that sends no fetch metadata even to loopback
(Safari before 16.4) only gets the `Origin` check there, which an image or link
does not trigger; set a bearer token if you use one. The `Host`
allowlist refuses DNS-rebinding hostnames, and the JSON-only routes refuse the
content types a browser can send without a CORS preflight. The public sign-in
routes (`POST /auth/login`, `POST /auth/recovery`) accept only
`application/json` bodies too, so a page on another site cannot sign your
browser in or try passwords. Images in wiki pages
rendered by the web UI may only use relative paths that stay inside the UI, so
viewing a stored page cannot make your browser call those routes either.

Busy shared hook servers can also set `AI_MEMORY_HOOK_RATE_PER_SEC` (tokens per
second per actor/session source) and optionally `AI_MEMORY_HOOK_RATE_BURST` to
bound one runaway session without blocking unrelated hook sources. Unset or `0`
rate leaves the limiter disabled.

To stop the server from storing particular tool calls whatever the client
sends, list them in `[ingest]`: `drop_tool_use_id_prefixes` (for example
`["toolu_plugin_"]`, the id prefix Claude Code gives plugin tool calls) and
`drop_tool_names` (exact names). The id and name are read from each
harness's own fields, such as OpenCode's `callID`. A matching PreToolUse or
PostToolUse event is acknowledged and discarded before it is sanitized,
rate-limited or stored. The lists apply to every project on the server.

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

**HTTPS.** ai-memory deliberately does not terminate TLS itself; put a
well-tested reverse proxy in front of it.
[`docs/https-via-proxy.md`](https-via-proxy.md) is the deployment
guide, with copy-paste docker compose templates in
[`docker/compose.tls.caddy.yml`](../docker/compose.tls.caddy.yml) (Caddy
with Let's Encrypt or internal CA) and
[`docker/compose.tls.cloudflared.yml`](../docker/compose.tls.cloudflared.yml)
(Cloudflare Tunnel, no open ports). Both are recommended once you
turn on multi-user or bind beyond loopback. The Quick Start default
of single-user on loopback doesn't need TLS; the guide says so
explicitly so you don't add setup you don't need.

**Multi-user attribution (v0.8, optional) plus human login.** When more
than one human shares a server, ai-memory attributes each write to a
named user. Humans sign in with username/password; agents and CLIs use
`Authorization: Bearer` (`AI_MEMORY_AUTH_TOKEN` for root automation, or
an `aim_` key from `ai-memory api-key add`). Data stays single-tenant and
there is no per-page RBAC, though a project can be **restricted** so only root,
its creator, and users with an explicit grant reach it (see
[per-project authorization](design-per-project-authz.md)). A
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
