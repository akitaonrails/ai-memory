# HTTPS via a reverse proxy

> ai-memory does **not** terminate TLS itself, by design. This page is
> the operator's guide to fronting it with a mature TLS terminator
> (Caddy, Cloudflare Tunnel, nginx) so tokens and `/web` cookies
> travel encrypted between clients and the server. Default install
> stays plain HTTP on loopback — no change for existing users on
> upgrade.

## When you don't need this

Skip TLS entirely if you're in one of these shapes — the security
budget is better spent elsewhere:

- **Single-user, stdio MCP transport.** `claude mcp add ai-memory -- ai-memory serve --transport stdio` never touches the network. No TLS to worry about.
- **Loopback-only HTTP server**, single user, no `/web` access from another machine. `127.0.0.1:49374` is unreachable from outside the host; TLS protects nothing here that the kernel's loopback boundary doesn't already.
- **Local dev / one-off experiments.** Bring TLS in when the deployment shape calls for it; not before.

The single-user happy path documented in the README's Quick Start is
this case. Most ai-memory installs never need a proxy.

## When you do need this

Add a TLS-terminating proxy in front of ai-memory when any of these
apply:

- **Multi-user mode is on** (at least one user row exists; `[auth].token_pepper`
  is the credential prerequisite). Per-user tokens travel between clients and
  the server — sniffable over plain HTTP on the LAN. See
  [`docs/users.md`](users.md).
- **The server is bound beyond loopback** (`AI_MEMORY_BIND=0.0.0.0:49374` or a LAN-routable IP). Anyone on the network segment sees plaintext token traffic and `/web` cookies.
- **You access `/web` from a different machine** than the one running ai-memory. The browser session cookie set after Basic auth lives in the clear over HTTP.
- **You're exposing ai-memory beyond the LAN.** Cloudflare Tunnel or a public-domain Caddy with Let's Encrypt are the two patterns most homelab operators land on.

ai-memory refuses to start unauthenticated non-loopback HTTP by default.
Configure `AI_MEMORY_AUTH_TOKEN` or bind loopback; the dangerous
`--allow-insecure-no-auth` escape hatch exists only for intentional plain-HTTP
LAN use. Authenticated non-loopback plain HTTP remains available and logs a
loud warning: bearer tokens and `/web` cookies are still sniffable without
TLS. The decision to add TLS is yours; this page is the recipes.

## Pick a path

| Path | Best for | What's needed externally |
|---|---|---|
| **Caddy + public domain + Let's Encrypt** | Operators with a domain name + port 80/443 reachable from the internet (most homelabs behind a forwarding router). | DNS A/AAAA record pointing at your IP. |
| **Caddy + internal CA (LAN-only)** | LAN-only multi-user, no public exposure. Each client machine has to trust Caddy's root cert once. | One-time root cert install per client. |
| **Cloudflare Tunnel** | "I don't want to open ports on my router" — outbound-only tunnel, TLS terminated at Cloudflare's edge. | A Cloudflare account (free tier works) + a domain on Cloudflare. |
| **External cert files (Caddy or nginx)** | You already have a corporate or homelab CA issuing certs to your services. | The cert/key files, however your environment produces them. |
| **nginx** | You already run nginx for other services and want one config language. | Same as Caddy: a domain or files. |

The compose templates in `docker/` are ready to copy:

- [`docker/compose.tls.caddy.yml`](../docker/compose.tls.caddy.yml) — Caddy front, both LE and internal-CA variants documented inline.
- [`docker/compose.tls.cloudflared.yml`](../docker/compose.tls.cloudflared.yml) — Cloudflare Tunnel sidecar, zero open ports.

The sections below walk through each.

---

## Path 1 — Caddy + public domain + Let's Encrypt

Cleanest path when you have a domain and port 80/443 reachable. Caddy
auto-issues + auto-renews from Let's Encrypt with no operator
involvement after first start.

### Compose template

Copy `docker/compose.tls.caddy.yml` to your deploy directory. The
relevant block is:

```yaml
services:
  ai-memory:
    image: akitaonrails/ai-memory:latest
    container_name: ai-memory
    restart: unless-stopped
    expose:
      - "49374"          # internal only — Caddy reaches it over the docker network
    volumes:
      - ai-memory-data:/data
    env_file:
      - .env.production  # AI_MEMORY_AUTH_TOKEN + AI_MEMORY_ALLOWED_HOSTS + your LLM provider creds

  caddy:
    image: caddy:2-alpine
    container_name: ai-memory-caddy
    restart: unless-stopped
    ports:
      - "80:80"           # for Let's Encrypt HTTP-01 challenges
      - "443:443"         # the only port your clients touch
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy-data:/data
      - caddy-config:/config

volumes:
  ai-memory-data:
    name: ai-memory-data
  caddy-data:           # cert + ACME account key live here. Back this up.
  caddy-config:
```

### Caddyfile

A complete one, three lines that actually matter:

```caddyfile
memory.example.com {
    reverse_proxy ai-memory:49374
}
```

Caddy will:

1. Solve the HTTP-01 ACME challenge on first request to that hostname.
2. Issue a Let's Encrypt cert.
3. Renew automatically 30 days before expiry.
4. Forward `Authorization: Bearer ...` headers (and your auth flow) untouched.
5. Set `X-Forwarded-Proto: https` and `X-Forwarded-For: <client-ip>` automatically.

### ai-memory `.env.production` adjustments

```bash
AI_MEMORY_AUTH_TOKEN=...long-random-token-from-generate-auth-token...
AI_MEMORY_AUTH__SECURE_COOKIE=true
AI_MEMORY_ALLOWED_HOSTS=memory.example.com,localhost,127.0.0.1
AI_MEMORY_BIND=0.0.0.0:49374
```

The `AI_MEMORY_ALLOWED_HOSTS` must include the public hostname or
ai-memory's DNS-rebinding guard will refuse Caddy's forwarded
requests.

### Hosting under a subpath

If ai-memory shares a hostname with other apps, keep the prefix when proxying
and tell ai-memory about it:

```bash
AI_MEMORY_BASE_PATH=/wiki
```

```caddyfile
memory.example.com {
    handle /wiki/* {
        reverse_proxy ai-memory:49374
    }
}
```

Do **not** use `handle_path /wiki/*` for this deployment: it strips `/wiki`
before forwarding, while ai-memory intentionally serves all routes under the
configured prefix. With the example above, clients use:

```bash
ai-memory install-mcp   --client claude-code --apply \
    --server-url "https://memory.example.com/wiki/mcp" --auth-token "$AI_MEMORY_AUTH_TOKEN"
ai-memory install-hooks --agent  claude-code --apply \
    --server-url "https://memory.example.com/wiki" --auth-token "$AI_MEMORY_AUTH_TOKEN"
```

The built-in browser is then at `https://memory.example.com/wiki/web`; add
`AI_MEMORY_WEB_SLUG=/` if you want the browser or custom `--web-ui-dir` SPA at
`https://memory.example.com/wiki` itself.

**Safety rules on both flags.** `AI_MEMORY_BASE_PATH` and
`AI_MEMORY_WEB_SLUG` go through the same normaliser. Segments must be
RFC 3986 unreserved characters (`[A-Za-z0-9-._~]`). Dot-segments
(`.` / `..`) are rejected — they mean "current" and "parent" at a
segment boundary, so accepting them would let a typo turn the prefix
into traversal. Anything outside the unreserved set falls back to a
root mount, and the startup log says why. The trailing-slash redirect
at `{base_path}{web_slug}/` keeps the query string on its way to the
canonical form.

### MCP client config (Claude Code shown — others follow the same shape)

```bash
ai-memory install-mcp   --client claude-code --apply \
    --server-url "https://memory.example.com/mcp" --auth-token "$AI_MEMORY_AUTH_TOKEN"
ai-memory install-hooks --agent  claude-code --apply \
    --server-url "https://memory.example.com" --auth-token "$AI_MEMORY_AUTH_TOKEN"
```

`https://` flips on, the token rides in `Authorization: Bearer`, and
Caddy's cert is browser/curl/MCP-client trusted everywhere because
Let's Encrypt is in every system trust store.

### What can go wrong

- **Port 80 not reachable from the internet** → ACME fails. Symptom: Caddy logs `Get "https://acme-v02.api.letsencrypt.org/...": ...` errors. Fix: forward 80 and 443 from your router to the Caddy host, OR switch to Cloudflare Tunnel (Path 3) which doesn't need open ports.
- **DNS not propagated yet** → first cert issuance fails with `unauthorized: ...DNS name does not have any address`. Fix: wait, or check the A record points at your public IP.
- **Cert renews silently fail months later** → Caddy logs the failure but you don't read Caddy logs. Fix: subscribe to `journalctl -u docker-compose@... | grep -i 'renew\|error'` or front Caddy with healthchecks.

---

## Path 2 — Caddy with internal CA (LAN-only)

You don't have a public domain or you don't want to expose anything
to the internet. Caddy's internal CA generates a per-server root cert
the operator installs **once** into each client machine's OS trust
store. Same wire shape as Path 1, no internet dependency, no port
forward.

### Caddyfile

```caddyfile
{
    local_certs   # tells Caddy to use the internal CA instead of LE
}

homelab.local, 192.168.1.50 {
    reverse_proxy ai-memory:49374
}
```

List every name + IP clients will use (browser, MCP client, curl)
in the site address. Caddy puts all of them in the cert's SAN.

### The trust-install step (the load-bearing one)

Caddy's root cert lives at `<caddy-data>/caddy/pki/authorities/local/root.crt`
inside the volume. Extract it once:

```bash
docker compose exec caddy cat /data/caddy/pki/authorities/local/root.crt > caddy-root.crt
```

Then install it into each client OS's trust store:

| OS | Command |
|---|---|
| macOS | `sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain caddy-root.crt` |
| Linux (Debian/Ubuntu) | `sudo cp caddy-root.crt /usr/local/share/ca-certificates/ && sudo update-ca-certificates` |
| Linux (Arch/openSUSE) | `sudo trust anchor --store caddy-root.crt` |
| Windows | `certutil -addstore -f "Root" caddy-root.crt` (Administrator PowerShell) |
| iOS / Android | Email the file to the device, open it, install as a profile in Settings → General → VPN & Device Management. Then **also** explicitly trust it under Settings → General → About → Certificate Trust Settings. |

**The warning that has to be loud**: if you skip the trust-install
step on a client, that client will either refuse TLS connections
(MCP clients, curl) or train the operator to click through warnings
(browsers). In the latter case **you have neither HTTP's transparency
nor HTTPS's protection** — you have a security theatre cert that
makes everyone less safe. Install the root cert on every client
machine you connect from, or use Path 1 / Path 3 instead.

### Same `.env` + client config shape as Path 1

Substitute `https://homelab.local` (or whichever SAN you set) for the
public domain. Everything else is identical.

---

## Path 3 — Cloudflare Tunnel

Cloudflare's `cloudflared` daemon establishes an outbound-only tunnel
to Cloudflare's edge. **No ports open on your router**, no public IP
needed, TLS terminated at the Cloudflare edge with their cert. Pairs
particularly well with the homelab multi-user case because the trust
story is "Cloudflare is the CA" — universally trusted, no per-client
install dance.

### One-time Cloudflare setup

1. Have a domain on Cloudflare (the registrar can be elsewhere; the DNS must be on Cloudflare).
2. In the Cloudflare dashboard, go to **Zero Trust → Networks → Tunnels** → **Create a tunnel** → name it `ai-memory-homelab` (or whatever) → save.
3. Cloudflare gives you a long token string. Save it for the compose file.
4. Add a public hostname to the tunnel: `memory.example.com` → service `http://ai-memory:49374`. Save.
5. (Optional but recommended) Wrap the hostname in a **Cloudflare Access** application — Cloudflare's zero-trust SSO sits in front of the tunnel and you get human auth via Google/GitHub/etc. **on top of** ai-memory's bearer token.

### Compose template

Copy `docker/compose.tls.cloudflared.yml`. The relevant block:

```yaml
services:
  ai-memory:
    image: akitaonrails/ai-memory:latest
    container_name: ai-memory
    restart: unless-stopped
    expose:
      - "49374"          # tunnel reaches it over the docker network — no host port
    volumes:
      - ai-memory-data:/data
    env_file:
      - .env.production

  cloudflared:
    image: cloudflare/cloudflared:latest
    container_name: ai-memory-tunnel
    restart: unless-stopped
    command: tunnel --no-autoupdate run
    environment:
      - TUNNEL_TOKEN=${CLOUDFLARE_TUNNEL_TOKEN}

volumes:
  ai-memory-data:
    name: ai-memory-data
```

`CLOUDFLARE_TUNNEL_TOKEN` goes in your `.env` (or compose env). No
ports exposed on the host. No DNS configuration beyond the dashboard
step above (Cloudflare manages the CNAME automatically).

### ai-memory `.env.production` adjustments

```bash
AI_MEMORY_AUTH_TOKEN=...long-random-token...
AI_MEMORY_AUTH__SECURE_COOKIE=true
AI_MEMORY_ALLOWED_HOSTS=memory.example.com,localhost,127.0.0.1
AI_MEMORY_BIND=0.0.0.0:49374
CLOUDFLARE_TUNNEL_TOKEN=eyJ...long-base64-from-the-cf-dashboard...
```

### Client config

Same as Path 1:

```bash
ai-memory install-mcp   --client claude-code --apply \
    --server-url "https://memory.example.com/mcp" --auth-token "$AI_MEMORY_AUTH_TOKEN"
```

### What can go wrong

- **Token leak**. Anyone with `CLOUDFLARE_TUNNEL_TOKEN` can run a tunnel for your hostname. Keep the env file `0600`, don't commit it.
- **Tunnel down + cf cached old DNS** → Cloudflare returns 502 for a few minutes after restart. Usually self-heals.
- **Access policies confused with bearer auth**. Cloudflare Access (the optional SSO layer) is a separate layer from ai-memory's bearer token. Both run; both must pass. If Access blocks a request, ai-memory never sees it.

---

## Path 4 — External cert files (Caddy or nginx)

You already have a CA issuing certs to your services (corporate PKI,
homelab Vault, anything). You don't want Caddy issuing its own.

### Caddyfile

```caddyfile
memory.example.com {
    tls /etc/caddy/certs/memory.crt /etc/caddy/certs/memory.key
    reverse_proxy ai-memory:49374
}
```

Mount the cert + key:

```yaml
services:
  caddy:
    # ... rest as Path 1 ...
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - /your/cert/path:/etc/caddy/certs:ro   # the cert dir
      - caddy-data:/data
```

Caddy hot-reloads the cert when files change. No reload required.

### nginx equivalent

```nginx
server {
    listen 443 ssl http2;
    server_name memory.example.com;

    ssl_certificate     /etc/nginx/certs/memory.crt;
    ssl_certificate_key /etc/nginx/certs/memory.key;

    location / {
        proxy_pass http://ai-memory:49374;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        # MCP Streamable HTTP transport is request-response; chunked
        # bodies and SSE both rely on the next two lines.
        proxy_http_version 1.1;
        proxy_set_header Connection "";
    }
}
```

The `proxy_http_version 1.1` + empty `Connection` are required for
MCP's Streamable HTTP transport to stream correctly.

---

## Native (non-Docker) Caddy

For operators running ai-memory from source / AUR / `cargo run`
without Docker:

```caddyfile
memory.example.com {
    reverse_proxy 127.0.0.1:49374
}
```

Install Caddy natively (`brew install caddy` / `pacman -S caddy` /
`apt install caddy`), drop the Caddyfile at the OS-canonical path
(`/etc/caddy/Caddyfile` on Linux, `/opt/homebrew/etc/Caddyfile` on
macOS), and `systemctl enable --now caddy` / `brew services start
caddy`. Everything else (LE, internal CA, external certs) works the
same as the Docker paths above — Caddy doesn't care which side of the
container boundary it's on.

For Cloudflare Tunnel: `cloudflared service install ${CLOUDFLARE_TUNNEL_TOKEN}`
installs and starts the tunnel as a systemd service on Linux or a
LaunchDaemon on macOS. Same shape as the Docker variant.

---

## What ai-memory does to support being behind a proxy

Nothing special — the server intentionally generates no absolute URLs
in responses, so it doesn't matter whether `https://` or `http://`
sits in front. The bearer token middleware reads `Authorization`
directly off the request, which proxies forward verbatim. The
`/api/v1` ETag is computed from request-independent fields.

For browser access to `/web` through HTTPS, set
`AI_MEMORY_AUTH__SECURE_COOKIE=true` (or `[auth] secure_cookie = true`). This
marks the Basic-auth session cookie `Secure`; it is always `HttpOnly`,
`SameSite=Strict`, and `Path=/`. ai-memory intentionally does **not** infer
HTTPS from `X-Forwarded-Proto` or any other proxy header. Close direct HTTP
access to the public hostname, or redirect it to HTTPS. Enabling
`secure_cookie` on a direct HTTP deployment makes browsers withhold the cookie,
which is expected safety behavior. It remains false by default so loopback and
plain-HTTP local `/web` continue to work.

The only thing to mind: **`AI_MEMORY_ALLOWED_HOSTS` must include the
public hostname**, not just `localhost`. The host-allowlist middleware
runs before any header rewriting, so it sees the proxy's forwarded
`Host: memory.example.com` and would reject it otherwise.

## Don't paper over the security gap

Three things to actively avoid:

1. **Don't disable the allowed-hosts guard.** It's the DNS-rebinding defence; pruning it because the proxy "should be" filtering is exactly the kind of "the other layer handles it" assumption that ships bugs. Add the public hostname; don't widen to `*`.
2. **Don't skip the trust-install step in Path 2.** The temptation is to add `-k` (curl) or `--insecure` (MCP clients that support it) "just to get it working." If you do, you have a security theatre cert: TLS without authentication, which is worse than HTTP with the bearer because it looks safe and isn't.
3. **Don't run cloudflared with `--no-tls-verify`.** Cloudflare's tunnel daemon validates ai-memory's cert by default — which is fine because ai-memory is on plain HTTP inside the docker network. Don't override the flag; you'd be reaching for it because something else is misconfigured.

If you can't take one of these paths cleanly, the honest answer is
"keep ai-memory loopback-only" or "front it with the proxy you
already trust." The configuration that gives operators the wrong
mental model — looking secure, not being secure — is worse than
either.

## The session-aware MCP bridge and HTTPS

`ai-memory mcp-bridge` reaches `https://` server URLs. Earlier releases could not:
its transport pulled in a second `reqwest` with no TLS backend compiled, so any
non-`http` scheme was refused before a connection was attempted. If you front
ai-memory with a TLS-terminating proxy as described above, point the bridge at the
proxied `https://` URL directly — a second, local proxy on each client machine is
not needed.

The bridge uses the platform certificate verifier, so it trusts the same roots the
operating system does. A certificate the OS does not trust — a self-signed one, or a
private CA that has not been installed into the system trust store — is rejected.
