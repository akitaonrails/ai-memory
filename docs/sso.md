# SSO / enterprise identity

This page consolidates ai-memory's existing OIDC support into one place for
teams whose IT-security policy requires centralized identity (SSO) for any
tool touching a workstation, before they can approve local install. It adds
no new behavior: everything here already exists in
[`docs/install.md`](install.md) and [`SECURITY.md`](../SECURITY.md). This
page answers "does it support our IdP?" without reading either in full.

## What's supported today

`ai-memory auth login oidc-device` performs an OIDC device-authorization
flow against any standards-compliant issuer (Keycloak, Okta, Entra ID, etc.):

```bash
ai-memory auth login oidc-device \
  --issuer "https://issuer.example.com/realms/team" \
  --client-id "ai-memory-cli"
```

The resulting access token is stored per-developer and used for two things:

- **Native lifecycle-hook auth**: the client-side hook that reports coding-
  session events to the server.
- **Thin-client HTTP commands** (`status`, `search`, `read-page`,
  `write-page`, `backup`, `embed`, and similar), when no static
  `AI_MEMORY_AUTH_TOKEN` / `[auth].bearer_token` is configured. Static bearer
  auth takes precedence if both are set.

See [`docs/install.md`](install.md) (the `auth login oidc-device` section)
for the full command reference and provider setup notes.

## What this is not

- It is **not** a login gate for the ai-memory server process itself. The
  server's own admin/root authorization model is the static root bearer
  token / database-user token ladder described in `SECURITY.md`'s
  "Authentication and administrative authorization" section. OIDC identifies
  the *human developer* to the CLI/hooks; it does not replace server-side
  admin auth.
- If you front the server with an OIDC-aware gateway or reverse proxy, that
  gateway can translate accepted OIDC identity into the auth ai-memory
  understands (static bearer / DB-user token). ai-memory itself does not
  validate OIDC tokens against your IdP for server API access.
- OIDC/Keycloak `sid` (session) claims describe the identity provider's login
  session, not the coding-agent session ai-memory tracks for per-session
  isolation (`[auto_scope]`). Don't conflate the two when wiring a gateway.

## Deployment shape for SSO-required environments

1. Set up your OIDC issuer's device-authorization flow (most enterprise IdPs
   support this out of the box for CLI tools).
2. Have each developer run `ai-memory auth login oidc-device` once, pointed
   at your issuer.
3. If you need the *server's* HTTP/admin surface behind your IdP too (not
   just the CLI/hooks), put an OIDC-aware reverse proxy or gateway in front
   of it and translate accepted identities into ai-memory's static bearer /
   DB-user tokens. See `SECURITY.md`'s network-exposure guidance and
   [`docs/https-via-proxy.md`](https-via-proxy.md) for the TLS-termination
   side of that setup.

## Related documents

- [`docs/install.md`](install.md): full `auth login oidc-device` reference.
- [`SECURITY.md`](../SECURITY.md): server-side authentication/authorization
  model.
- [`DATA_HANDLING.md`](../DATA_HANDLING.md): what data moves where.
