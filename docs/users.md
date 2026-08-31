# Multi-user attribution

> **Status:** Introduced in v0.8; this page documents the current shipped
> contract.

ai-memory is **single-tenant wiki data** with **optional multi-user
attribution**. Every authenticated request sees the same wiki pages —
there is no per-page RBAC or group permission model. Operational handoffs and
open-session recovery are owner-scoped so one operator cannot accidentally
consume or finalize another's live context. What multi-user mode also adds is
*who-did-this*: every write
attributes to a named user, audit-log rows carry that identity, and the
web UI can show "Last edited by Alice Smith" instead of the anonymous
default. Every `/admin/*` endpoint stays root-only once the deployment has a
DB user or trusted-proxy identities, including read-only
status/search/read-page helpers.

Hook session identifiers are also owner-bound. A hook authenticated as one
operator cannot reuse another operator's UUID to append events or trigger a
summary/handoff/end transition. Shared legacy sessions remain available to all
callers. Cross-owner recovery is limited to root's explicit
`finalize-session --all-owners` path.

Authenticated clients sharing one server must emit a distinct agent-run id for
each run and forward that same id on their MCP requests when using session-aware
auto-scope. Legacy sessions whose stored owner is `NULL` remain shared.

If you run ai-memory alone, you can skip this page — your install
keeps working unchanged.

## When to enable it

You probably want multi-user mode when:

- More than one human shares a single ai-memory server (a household,
  a small team's homelab).
- You want the audit log to record *who* made each write (e.g. to
  trace `Codex` writes vs `Claude Code` writes vs hand-rolled CLI
  calls).
- You're planning to use the admission webhook chain —
  webhooks receive the actor identity in their payload.

You probably **don't** need it when:

- You're the sole human user of your install. Single-user mode (no user rows)
  remains compatible whether or not `init` has generated `[auth].token_pepper`.
- You need permissions / access control. v1 of ai-memory does
  not implement RBAC by design (see
  [`design-decisions.md`](design-decisions.md) §13). Attribution
  records *who* did a write; it does not gate *whether* they
  could.

## The four resolution rungs

Every HTTP request is resolved to one of four authentication tiers:

| Rung | Trigger | What the request gets |
|---|---|---|
| **0 — Anonymous** | No `[auth].bearer_token` set. | Allowed, no identity. Same as pre-multi-user defaults. |
| **1 — Root** | Bearer matches `[auth].bearer_token`. | Allowed as **root**. When `[auth].root_username` is set, writes attribute to that name; otherwise attribution stays anonymous. |
| **1b — Proxy-asserted user** | Bearer matches the distinct `[auth].actor_proxy_bearer_token`. | Identity is taken from trusted `X-Memory-Actor-*` headers. The request is a **user** unless its OIDC issuer/subject pair exactly matches the configured root pair. Missing or malformed identity is rejected. |
| **2 — DB user** | Bearer doesn't match root, matches a `users.token_hash` row (via SHA-256 of token + `[auth].token_pepper`). | Allowed as **that user** for normal read/write APIs. All `/admin/*` endpoints are root-only in multi-user mode. The audit log records the username/email/name. |
| **3 — 401** | Bearer present but matches nothing. | Rejected. Closes the bypass — unknown bearers can't slip through as anonymous. |

The rungs are sticky: a request is matched at the first credential that
applies, never escalates. Startup rejects equal root and proxy credentials;
root and proxy credentials take precedence over any accidental DB-token
collision.

## Trusted proxy identity

A deployment that terminates SSO validates the end user's credential, then
authenticates upstream with a proxy-only bearer and describes the human in
`X-Memory-Actor-*` headers. Those headers are **ignored by default** on root
and DB-user requests because anything that can reach the port could otherwise
claim any identity.

Configure a distinct proxy credential:

```toml
[auth]
bearer_token = "<root-token>"                    # direct administration
actor_proxy_bearer_token = "<different-token>"  # SSO proxy only
secure_cookie = true                              # when `/web` is HTTPS-only

# Optional stable identity for the root human behind an OIDC proxy.
root_issuer = "https://idp.example"
root_subject = "<root-subject>"
```

- The proxy token **is** the switch. A blank value counts as unset. It must
  differ from `bearer_token`, and `serve` refuses startup otherwise or when the
  root bearer is absent.
- Only set it when the server is reachable *only* through that proxy.
- `secure_cookie` is independent of proxy identity. Enable it when a trusted
  reverse proxy terminates HTTPS for `/web`; ai-memory never trusts forwarded
  protocol headers to infer it. Direct HTTP browsers will not send that cookie.
- **The proxy MUST strip client-supplied `X-Memory-Actor-*` headers before
  setting its own.** Use a directive that *replaces* the header rather than
  appending to it (nginx `proxy_set_header`, Traefik `customRequestHeaders`) —
  with an appending ingress the client's value arrives first and would be the
  one read. Repeated headers and comma-folded values are rejected with `400`
  rather than resolved to one identity.
- Every proxy request must assert `X-Memory-Actor-User`, or both
  `X-Memory-Actor-Issuer` and `X-Memory-Actor-Sub`. The OIDC fields are a pair:
  the standard guarantees subject uniqueness only within an issuer. A partial
  pair or a request that names nobody is rejected with `400`.
- Proxy callers are users by default. A username-only assertion can never
  become root, including one equal to `root_username`, because an OIDC display
  username is not a stable unique identifier. Proxied root access requires an
  exact match with `root_issuer` plus `root_subject`.
- Origin health checks or maintenance calls that need root should use the root
  bearer, not the proxy bearer. Raw actor headers on the root rung are ignored.

## Identity keys

Identity-sensitive routing uses a *qualified* identity, never a bare string.
`ActorContext::identity_key()` resolves a request to the OIDC
`(issuer, subject)` pair when both are present, or to `user:<name>` for a
username-only identity. The namespaces are disjoint, and identical subjects
from different issuers stay different.

The OIDC pair outranks `user` because OIDC defines `(iss, sub)` as the stable
identifier and explicitly forbids relying on `preferred_username` for
uniqueness. Configure the proxy to forward both values from day one. Adding a
display username later then preserves the same identity; moving from a
username-only assertion to the OIDC pair deliberately changes it once.

## Ownership of handoffs and sessions

Handoffs and sessions record the operator they belong to (`owner_user` /
`actor_user`, holding the qualified `IdentityKey::storage_key()` TEXT). On a
shared server this stops one operator's pending handoff from being delivered
to — and consumed by — the next session to start, whoever it belongs to.

- A `NULL` owner means **shared with the project**: every row written before
  ownership existed, and anything written without an authenticated actor, stays
  visible to everyone.
- **An owner is only stamped where the deployment distinguishes operators.**
  Single-operator servers are unaffected even when they name their operator via
  `[auth].root_username`: with no `users` rows and no proxy bearer there is
  nobody to separate, and stamping the one name would separate that operator's
  *transports* instead — HTTP requests carry the name, while the stdio /
  in-process MCP transport and the local CLI carry no actor at all and would
  stop seeing what the HTTP side wrote. Reads are deliberately **not** gated the
  same way, so a row stamped while the deployment did distinguish operators
  stays readable by that operator afterwards.
- The owner is the qualified identity the request names —
  `ActorContext::identity_key()`, so the issuer-qualified OIDC key when a
  complete issuer/subject pair is asserted and `user:<name>` otherwise. It is
  the same rule that decides the auth tier, so the proxy path gets real
  per-operator isolation rather than one shared bucket.
- `memory_handoff_begin` takes `shared: true` to publish a baton deliberately.
- `memory_handoff_accept` / `memory_handoff_cancel` take `any_owner: true` to
  act on somebody else's baton; that opt-out requires admin authority in
  multi-user mode.
- `ai-memory finalize-session --all-owners` does the same for sessions, and
  `GET /admin/open-sessions?all_owners=true` is the underlying switch.
  `--session-id <uuid>` / `session_id=<uuid>` narrows the same owner-scoped
  lookup to one exact open session; it cannot be combined with `--all` /
  `all=true`.
- `GET /admin/sessions/by-agent` reports how many sessions each agent CLI
  opened in a scope. It follows the same rule: the caller's own sessions
  plus the unowned ones, with `all_owners=true` to see every operator's. Pass
  the required `workspace` and `project` query parameters and optionally
  `since_days=N`; zero or omission means all history. Results use the stable
  shape `{"by_agent":[{"agent":"codex","sessions":3}]}`, ordered by count
  descending and then agent name. An unknown scope returns 404 without creating
  it. Like every `/admin/*` route, this endpoint is root-only when the
  deployment distinguishes operators.
- The read-only handoff listing (`GET
  /api/v1/workspaces/{ws}/projects/{p}/handoffs`) serves its prompt-derived
  fields — `summary`, `open_questions`, `next_steps` — to a caller the server
  can name (their own rows plus the shared ones) and to the root operator, who
  reads every page body through the wiki API anyway. A caller an authenticating
  server can place as neither gets the metadata with `redacted: true`. A server
  with no auth configured is unaffected: it already serves every page body
  unauthenticated. The default listing remains own plus shared; root may request
  the explicit recovery view with `?all_owners=true`, while user and anonymous
  tiers receive `403`.
- Handoff lifecycle events raise admission ops (`handoff_begin`,
  `handoff_accept`, `handoff_cancel`), so an admission webhook can observe or —
  with `failure_policy = "reject"` — refuse them. Only reject-policy hooks are
  awaited on these ops; observers are notified after the operation is durable.

"Multi-user mode" here means *the deployment distinguishes operators*: either
`users` rows exist, or `[auth].actor_proxy_bearer_token` is configured. A trusted
proxy never writes a `users` row, so counting only rows would leave every
proxied caller on the single-operator escape hatch that waves admin through.
One question, every gate: the MCP admin tools, the `/admin/*` route layer, and
the ownership stamped on handoffs and sessions all ask it.

## MCP client activity

`GET /admin/activity/by-client` reports server-wide MCP tool calls rather than
lifecycle sessions or project-owned data. Stateful HTTP and stdio use the
sanitized MCP `clientInfo.name`; stateless requests fall back to an
authenticated proxy's actor-agent label and then `unknown`. Results have the
stable shape
`{"by_client":[{"client":"claude-code","reads":12,"writes":3}]}` and are
ordered by total calls, then client name.

`since_days=N` includes every UTC day bucket intersecting that lookback; zero
or omission means all history. Calls flush on a one-minute background interval
even if no later request arrives, and retry failed batches once per interval;
process exit can lose the current interval. Each UTC day stores at most 128
distinct labels and folds additional labels into `other`. The endpoint takes
no workspace or project because many MCP-only clients do not provide reliable
per-call scope. Like every `/admin/*` route, it is root-only when the deployment
distinguishes operators.

## Per-operator memory slots

The "absent means shared" rule extends to memory slots, so a single-operator
server behaves exactly as it always has. `_slots/current-focus.md` is injected
into every operator's context; `_slots/<segment>/current-focus.md` is injected
only into the operator whose `path_segment()` is `<segment>` (`u-alice`,
`uh-<uuid>` for a mixed-case, trailing-period, or otherwise path-hostile
username, or `o-<uuid>` for a complete OIDC issuer/subject pair). What the
feature scopes is injection, not access: a slot is an ordinary wiki page, so
exact reads and searches remain project-wide like every other page. Every slot
written before this is unnamespaced, therefore shared.

`[slots] per_user` (default off) is the switch for the whole regime. With it
ON:

- session briefs and consolidation prompts show you the shared slots plus your
  own — including the pointer list of recently touched pages, so another
  operator's slot path and title stay out of your brief too;
- the engine namespaces the slots it writes: a consolidation run that targets
  the shared slot lands in the session operator's own namespace instead, and a
  path the model aims at somebody else's namespace is skipped rather than
  written or re-homed — that path comes from the model, and
  anything reaching your observations can dictate it;
- a `memory_write_page` call naming the SHARED slot is namespaced into your
  own prefix, exactly as the engine would (the response reports the path the
  page actually got), and writing into another operator's namespace is refused
  (admins may still curate any namespace, the shared slot included).

With it OFF a nested slot path means nothing in particular — every slot goes
into every brief, exactly as before the feature existed — so turning it back
off makes personal slots visible to everyone again rather than stranding them.

The `<segment>` is derived from the qualified identity on this server: a short,
lowercase, path-safe ASCII username stays readable, while a mixed-case,
trailing-period, or otherwise path-hostile username or complete OIDC
issuer/subject pair becomes a bounded deterministic identifier. Restricting
readable segments to lowercase without trailing periods prevents distinct
identities from producing pathnames that compare alike on supported
case-insensitive filesystems. The OIDC pair
outranks the username; see "Identity keys" above. Every named operator owns a
working namespace, and long or path-hostile values never fall back onto the
shared slot. One consequence of qualified segments is worth stating: a nested
path written before the feature
(`_slots/backend/…`) spells a segment no qualified identity can produce, so
with the flag ON it belongs to nobody and reaches no brief until the flag is
turned back off or an admin re-homes it.

Before the case-insensitive namespace fix, a mixed-case username such as
`Alice` used the readable segment `u-Alice`, and a username ending in a period
kept that period; both now use deterministic `uh-<uuid>` segments. ai-memory
cannot safely move the old directory automatically because a case-insensitive
filesystem may already have combined it with another identity. Administrators
upgrading a shared deployment with `[slots] per_user = true` must inspect any
affected `u-…` slot directories and re-home confirmed content into the owning
operator's new namespace. Preserve the old pages until ownership is
established; do not infer it from filename casing alone.

One gap is deliberate and documented rather than closed: `ai-memory bootstrap`
writes pages at paths the model picks from the repository's own README, docs
and code, with no operator to attribute them to, so a repo carrying injected
instructions can make it write a `_slots/…` page. It is an admin-only
operation on a repository the admin chose to ingest, and the behaviour is the
same with the flag off; review `bootstrap.md` — it lists every path written.

## Other per-operator state

Beyond attribution, some engine state is recorded per operator. "Absent means
shared" is the rule throughout — a row with no recorded operator behaves
exactly as it did before the column existed — so a single-operator server
keeps its historical behaviour:

- **Auto-improvement proposals.** Each records the operator who staged it (the
  qualified identity key — username or complete OIDC issuer/subject pair — so
  proxy-asserted humans count too, and it shows up on the proposal detail),
  and the "one
  pending proposal per page" rule applies per operator, so operators stop
  blocking each other. Only where the deployment distinguishes operators,
  though: elsewhere proposals stay unattributed and the original one-per-page
  rule holds unchanged. A scheduled run has no caller and stages unattributed;
  the telemetry report and the curator describe the project rather than a
  person and stay unattributed too, so they neither block nor are blocked by
  any named operator's pending proposal for the same page.

  A proposal that does collide with one already pending is skipped on its own
  — the run's other proposals still stage — and every staging surface reports
  the skip with the target path and the reason (the `skipped` list in the MCP
  and `/admin` responses, the CLI output, and the scheduler's log), so a run
  of N-1 proposals is never silently indistinguishable from a clean run of
  N-1.
- **Page reinforcement.** The first reinforced read by each identified
  operator is recorded per page alongside the existing shared access counter.
  `[decay] breadth_weight` (default `0.0`) optionally lets a
  page reinforced by many different people outrank one read repeatedly by a
  single person — the forget sweep reads the per-page count of distinct
  operators and feeds it into the retention score. At the default, and for
  pages with fewer than two distinct readers at any weight, retention scores
  are unchanged.

## Implementation contract

Request identity and authorization are separate:

- `ActorContext` carries who made the request and is used for attribution,
  frontmatter, audit payloads, and active-project keys.
- `AuthLevel` carries what auth tier the middleware resolved.
- `AuthLevel::authorize(Capability::...)` is the shared permission check for
  admin routes, user-management routes, normal read/write surfaces, and the
  admission-chain skip header.

Handlers should not compare usernames, infer root from `ActorContext`, or add
ad hoc root-only branches. PRs that touch auth behavior should cover root,
DB-user, and anonymous callers, including the single-user compatibility mode
where `[auth].token_pepper` is absent.

## Quick start

> Prerequisite: a fresh `ai-memory init`. Pre-v0.8 installs need
> the [migration step](#migrating-an-existing-single-user-install)
> below before any of these commands work.

### 1. Set the root identity

Edit your `config.toml` (typically `<data_dir>/config.toml` or
`/etc/ai-memory/config.toml`) and uncomment the `root_*` lines in
the `[auth]` block:

```toml
[auth]
bearer_token = "<your-existing-token-or-a-fresh-one>"
token_pepper = "<auto-generated-by-ai-memory-init>"

root_username = "boss"            # required for root attribution
root_email    = "boss@example.com" # optional, surfaced in UIs
root_name     = "Boss"             # optional, surfaced in UIs
```

`token_pepper` was auto-generated by `ai-memory init`; **do not
change it after adding users** — rotating the pepper invalidates
every existing token. The pepper is what makes a stolen `users`
table useless to an offline attacker; an attacker with both the
DB and the config has tokens at their disposal anyway, so the
pepper's job is closed by the file-permission boundary.

`init` creates the pepper before any users exist. Until the first user is
added, operational admin endpoints retain single-user compatibility; creating
that first user switches them to root-only immediately, without a restart.
Expired user rows still keep admin mode root-only. If a database has users but
either the pepper or static root bearer is missing or blank, `serve` refuses
startup. Restore both original secrets from configuration backup (or set the
root bearer from the secret manager) rather than removing users; the root token
is required to administer the existing users.

### 2. Add another user

Each `ai-memory user add` issues one token, printed **exactly
once**. Only its SHA-256 digest is kept in the DB.

```console
$ AI_MEMORY_AUTH_TOKEN=<root-token> \
  ai-memory user add --username alice --email alice@home --name "Alice Smith"

✓ created user 'alice'
  name:  Alice Smith
  email: alice@home
  id:    01935a82-6f7a-7d22-b8c0-...

Store this token now — it will NOT be shown again. Only its
SHA-256 digest is kept in the DB.

mYi3pq...<43-chars>...wKp2Ze
```

stderr carries the human chrome, stdout carries the bare token
so you can pipe it (`> ~/.config/ai-memory/alice.token`).

### 3. List users

```console
$ AI_MEMORY_AUTH_TOKEN=<root-token> ai-memory user list

USERNAME  NAME         EMAIL             STATUS
alice     Alice Smith  alice@home        active
bob       -            bob@home          active
carol     -            -                 expired
```

The list never surfaces tokens — only their hashes are in the DB.

### 4. Disable a token (without losing attribution history)

`ai-memory user expire <username>` stamps `token_expired_at = now()`
on the row. The user's bearer stops authenticating immediately, but
the row stays put so historical `author_id` references in
`audit_log` and `pages` keep resolving to their
real names.

```console
$ ai-memory user expire alice
Expire token for user 'alice'? Their token stops authenticating immediately. (y/N) y
✓ expired token for user 'alice'
```

Pass `--yes` to skip the prompt (CI / scripts).

To re-enable later: `ai-memory user revive alice`.

### 5. Rotate a leaked / lost token

```console
$ ai-memory user rotate-token alice
Rotate token for user 'alice'? Any existing client using the old token will start getting 401 immediately. (y/N) y
✓ rotated token for user 'alice'

Store this token now — it will NOT be shown again.

XGqsBp...<43-chars>...zRm0Vt
```

Rotation implicitly revives an expired token — you can recover an
offboarded user without first running `revive`.

#### What rotation does to already-spooled hook events

A lifecycle hook that cannot reach the server writes the event to the local
spool **together with the bearer it was holding at the time**. Rotating a token
therefore strands every spooled event captured under the old one: each is
rejected with `401` on the next drain.

Since #542 the drain recovers them. Re-run `install-hooks --apply` with the new
token, and the next drain retries any `401`ed entry with the current bearer —
the hook that spawns the drain hands it down in the child's environment. The
retry is bounded: it only runs for an entry the server has already rejected,
only for a static bearer (an OIDC token is re-resolved and refreshed every pass
anyway), and only when the current token actually differs from the one that
just failed.

Until you re-run `install-hooks`, the hooks still hold the old token and there
is nothing newer to retry with — those events stay queued and age out on the
normal retry budget rather than blocking the rest of the spool (#493).

## Backward compatibility

If you're upgrading from a pre-v0.8 ai-memory:

- **No action is required.** Your existing
  `[auth].bearer_token`-only setup continues to authenticate
  exactly as before. The auth middleware just stamps an anonymous
  `ActorContext` and your audit log records the same shape it did
  before.
- The `users` table is added by migration V14 and stays empty
  until you actively run `ai-memory user add`. SQL queries against
  it return no rows; the rest of the schema is unchanged.
- Multi-user mode requires `[auth].token_pepper`. Without it, the
  user-management endpoints return **503** with a clear
  `multi-user not enabled` message. Existing installs never trip
  this because they never call `user add`.
- `/admin/*` endpoints are open to the configured bearer token in
  single-user mode, matching historical behavior. Creating the first user row
  immediately makes every admin endpoint root-only; DB-user tokens receive
  **403** and anonymous requests receive **401**. Merely configuring
  `[auth].token_pepper` does not activate that boundary.

### Migrating an existing single-user install

`ai-memory init` is idempotent and won't overwrite a config it
finds. To populate `token_pepper` without losing your current
config:

1. **Back up the existing config** (`cp config.toml config.toml.bak`).
2. **Generate a pepper**: `ai-memory generate-auth-token 32` — this
   prints a hex string of the same shape `init` would have
   generated.
3. **Add the `[auth]` block** to your `config.toml`:

   ```toml
   [auth]
   # ... your existing settings (bearer_token, etc.) ...
   token_pepper = "<paste-the-generated-pepper-here>"
   root_username = "boss"     # optional; enables root-token attribution
   root_email    = "boss@..." # optional
   root_name     = "Boss"     # optional
   ```

4. Restart `ai-memory serve`. The new fields are picked up; existing
   behaviour is unchanged.

You can defer steps 3-4 indefinitely — `bearer_token` alone keeps
working as it always has.

## How tokens are stored

- 32 bytes of OS CSPRNG, URL-safe-base64-encoded → 43-character
  string.
- DB column `users.token_hash` stores `SHA-256(token || ":" ||
  token_pepper)`, never the plaintext.
- The per-server `token_pepper` makes a DB-only theft (e.g. a
  copied SQLite file) useless to an offline attacker: the search
  space for the unpeppered hash is `(token, pepper)` jointly.
- Constant-time comparison (`subtle::ConstantTimeEq`) on the hash
  side-steps timing attacks against the lookup path.

We deliberately **don't** use argon2id here even though it would be
the textbook choice. Tokens are 256-bit CSPRNG, so brute force is
infeasible regardless of hash strength; argon2id's per-hash salt
would force O(N) scans on every auth request, where SHA-256 +
`UNIQUE` index gives us the O(1) lookup the hot path needs.
See `crates/ai-memory-store/src/users.rs` for the full rationale.

## Where attribution shows up

| Surface | Status |
|---|---|
| Auth middleware injects `Extension<ActorContext>` on every request | ✓ P1.3 |
| All `/admin/*` routes gate on `Extension<AuthLevel>::Root` in multi-user mode | ✓ P1.4 |
| `ai-memory user add/list/expire/revive/rotate-token` CLI | ✓ P1.5 |
| `pages.author_id` populated, frontmatter `last_modified_by` block | ✓ P1.6 |
| `/api/v1` page responses include `author: { username, name?, email? }` | ✓ P1.7 |
| ETag invalidation on author change (so caches refresh attribution) | ✓ P1.7 |
| `install-hooks --as-user <name>` metadata + flag validation | ✓ P1.8 |
| Web UI shows author on the page view | ✓ shipped |
| Attributed mutation audit rows carry `audit_log.author_id` | ✓ shipped |

Commit ids for each milestone are recorded in `CHANGELOG.md`.

## Wiring agent hooks to a specific user

After `ai-memory user add` prints a user's token, point that user's
agent install at it via `install-hooks`:

```console
$ ai-memory user add --username alice --email alice@home --name "Alice Smith"
✓ created user 'alice'
  name:  Alice Smith
  email: alice@home
  ...

XGq...<43-chars>...zRm    # the token, stdout only

$ ai-memory install-hooks --apply --agent claude-code \
    --as-user alice --auth-token XGq...<43-chars>...zRm
[ai-memory] hooks installing for user: alice
✓ staged 5 hook script(s) → ...
```

`--as-user` is **metadata only**: it labels the install for the
operator's records and prints a confirmation line so you can verify
which identity the next session's writes will attribute to. The
actual token wired into the hook env block is whatever you pass via
`--auth-token`. Mismatching the two (e.g. `--as-user alice
--auth-token <bob's token>`) is permitted at the CLI layer; the
server will resolve to bob at runtime. The flag is there to keep the
operator honest, not to enforce.

Without `--as-user`, hooks install the same way they always have —
the bearer authenticates, attribution flows from the token's owner
(root user or DB user) at write time.

## Limitations

- **No per-page RBAC.** Every authenticated user sees every page in
  the workspace. All `/admin/*` endpoints are still root-only in
  multi-user mode. If you need data isolation, run separate
  ai-memory servers (per-user data dirs) and front them with a reverse
  proxy.
- **One token per user.** Rotation issues a new token and
  invalidates the old in the same transaction. There's no
  notion of multiple device-bound tokens per user.
- **Root token is single.** `[auth].bearer_token` is the admin token
  for every `/admin/*` endpoint. DB users created with `user add` are
  normal users, not additional admins.
- **OIDC is request authentication, not page authorization.** Native hooks and
  thin-client CLI commands can send a per-developer OIDC bearer for an external
  OIDC-aware gateway/bridge. Native ai-memory server auth still uses static root
  bearer / DB-user tokens, and `/admin/*` stays root-only unless a gateway
  translates accepted OIDC auth into upstream auth that ai-memory accepts.
  ai-memory still has one shared wiki per server and no
  per-page RBAC. The Keycloak/OIDC `sid` claim is also not an ai-memory agent
  session id; session auto-scope needs the lifecycle-hook session id or explicit
  `workspace` + `project` / `scopes`.
