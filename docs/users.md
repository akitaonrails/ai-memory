# Multi-user attribution

> **Status:** Introduced in v0.8; this page documents the current shipped
> contract.

ai-memory is **single-tenant data** with **optional multi-user
attribution**. Every authenticated request sees the same wiki pages —
there is no per-page RBAC, no per-user data scoping, no group
permissions. What multi-user mode adds is *who-did-this*: every write
attributes to a named user, audit-log rows carry that identity, and the
web UI can show "Last edited by Alice Smith" instead of the anonymous
default. Every `/admin/*` endpoint stays root-only once at least one user
row exists, including read-only status/search/read-page helpers.

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
| **1b — Proxy-asserted user** | Bearer matches root **and** the request carries `X-Memory-Actor-Proxy-Secret` matching `[auth].actor_proxy_secret`. | Identity is taken from the `X-Memory-Actor-*` headers. When the proxy names somebody, the tier drops to **user** unless that somebody is `[auth].root_username`; when it names nobody (health checks, machine-to-machine calls) the request stays **root**, as it would without the overlay. See "Trusted proxy identity" below. |
| **2 — DB user** | Bearer doesn't match root, matches a `users.token_hash` row (via SHA-256 of token + `[auth].token_pepper`). | Allowed as **that user** for normal read/write APIs. All `/admin/*` endpoints are root-only in multi-user mode. The audit log records the username/email/name. |
| **3 — 401** | Bearer present but matches nothing. | Rejected. Closes the bypass — unknown bearers can't slip through as anonymous. |

The rungs are sticky: a request is matched at the lowest tier that
applies, never escalates. Root token always wins over any users-row
collision; the two namespaces are intentionally distinct.

## Trusted proxy identity

A deployment that terminates SSO in front of the server usually cannot forward
the end user's own credential upstream: the proxy validates the token and then
authenticates to ai-memory with the single root bearer, describing the human in
`X-Memory-Actor-*` headers. Those headers are **ignored by default** — anything
that can reach the port could otherwise claim any identity — so without extra
configuration every human behind such a proxy collapses into one root actor.

Set `[auth].actor_proxy_secret` and have the proxy echo it in
`X-Memory-Actor-Proxy-Secret` to change that:

```toml
[auth]
bearer_token = "…"        # required: the overlay only applies to root-bearer requests
actor_proxy_secret = "…"  # shared with the proxy; compared in constant time
```

- The secret **is** the switch. There is no separate "trust headers" flag, so
  the feature cannot be enabled without one; a blank value counts as unset.
- Only set it when the server is reachable *only* through that proxy.
- **The proxy MUST strip client-supplied `X-Memory-Actor-*` headers before
  setting its own.** Use a directive that *replaces* the header rather than
  appending to it (nginx `proxy_set_header`, Traefik `customRequestHeaders`) —
  with an appending ingress the client's value arrives first and would be the
  one read. A request that carries any `X-Memory-Actor-*` header twice is
  rejected with `400` rather than resolved to one of the two identities, so a
  misconfigured ingress fails loudly instead of letting callers impersonate
  each other.
- `[auth].root_username` is **required** alongside the secret; the server
  refuses to start without it. Every asserted identity except that one is
  downgraded to a non-root user, so with no root operator named, nothing could
  ever reach `/admin/*` or create the first DB user.
- The asserted identity **replaces** the root template's user/name/email/sub as
  a block. A proxy that names nobody yields an unattributed actor rather than
  silently reusing the root username.
- The tier drops to **user**, so proxied humans do not inherit root capability.
  Only a caller the proxy names as `[auth].root_username` stays root. "Names
  somebody" covers any asserted identity field: an ingress that forwards only
  the subject claim (no `preferred_username`, so no `X-Memory-Actor-User`) has
  named a human who can never match `root_username`, and resolves at the user
  tier. Only a request asserting no identity at all stays root.
- Setting the secret without `bearer_token` logs a warning and does nothing.

## Ownership of handoffs and sessions

Handoffs and sessions record the operator they belong to (`owner_user` /
`actor_user`). On a shared server this stops one operator's pending handoff
from being delivered to — and consumed by — the next session to start, whoever
it belongs to.

- A `NULL` owner means **shared with the project**: every row written before
  ownership existed, and anything written without an authenticated actor, stays
  visible to everyone.
- **An owner is only stamped where the deployment distinguishes operators.**
  Single-operator servers are unaffected even when they name their operator via
  `[auth].root_username`: with no `users` rows and no proxy secret there is
  nobody to separate, and stamping the one name would separate that operator's
  *transports* instead — HTTP requests carry the name, while the stdio /
  in-process MCP transport and the local CLI carry no actor at all and would
  stop seeing what the HTTP side wrote. Reads are deliberately **not** gated the
  same way, so a row stamped while the deployment did distinguish operators
  stays readable by that operator afterwards.
- The owner is the identity the request names, which is the username when there
  is one and the asserted subject claim otherwise — the same rule that decides
  the auth tier, so a proxy that forwards only `X-Memory-Actor-Sub` gets real
  per-operator isolation rather than one shared bucket.
- `memory_handoff_begin` takes `shared: true` to publish a baton deliberately.
- `memory_handoff_accept` / `memory_handoff_cancel` take `any_owner: true` to
  act on somebody else's baton; that opt-out requires admin authority in
  multi-user mode.
- `ai-memory finalize-session --all-owners` does the same for sessions, and
  `GET /admin/open-sessions?all_owners=true` is the underlying switch.
- `memory_forget_sweep` requires admin authority in multi-user mode: it removes
  page versions permanently. It does fire the `forget_sweep` admission op, but
  the chain only refuses when an operator configured a reject-policy webhook for
  it, so it cannot stand in for the capability gate.
- The read-only handoff listing (`GET
  /api/v1/workspaces/{ws}/projects/{p}/handoffs`) serves its prompt-derived
  fields — `summary`, `open_questions`, `next_steps` — to a caller the server
  can name (their own rows plus the shared ones) and to the root operator, who
  reads every page body through the wiki API anyway. A caller an authenticating
  server can place as neither gets the metadata with `redacted: true`. A server
  with no auth configured is unaffected: it already serves every page body
  unauthenticated.

"Multi-user mode" here means *the deployment distinguishes operators*: either
`users` rows exist, or `[auth].actor_proxy_secret` is configured. A trusted
proxy never writes a `users` row, so counting only rows would leave every
proxied caller on the single-operator escape hatch that waves admin through.
One question, every gate: the MCP admin tools, the `/admin/*` route layer, the
ownership stamped on handoffs and sessions, and the per-operator bucketing of
pending auto-improvement proposals all ask it.

## Other per-operator state

The same "absent means shared" rule extends to three more places, so a
single-operator server behaves exactly as it always has:

- **Memory slots.** `_slots/current-focus.md` is injected into every operator's
  context; `_slots/<user>/current-focus.md` is injected only into that
  operator's. What the feature scopes is INJECTION, not access: a slot is an
  ordinary wiki page, so `memory_read_page`, `memory_query` and
  `memory_explore` return anyone's slot body to anyone who names its path, the
  same as every other page on the server. Every slot written before this is
  unnamespaced, therefore shared. `[slots] per_user`
  (default off) is the switch for the whole regime: with it ON the engine
  namespaces the slots it writes, session briefs and consolidation prompts show
  you the shared slots plus your own, and writing into someone else's namespace
  is refused (admins may still curate) — including a page the consolidator
  proposes, since that path comes from the model and anything reaching your
  observations can dictate it; such an update is skipped and reported back
  rather than re-homed. Every door onto a slot answers the same way, because a
  rule that holds at two doors of three holds nowhere: a `memory_write_page`
  call naming the SHARED slot is namespaced into your own prefix, exactly as
  the engine would (the response reports the path the page actually got), and an
  auto-improvement proposal that targets a slot is namespaced to the operator
  whose SESSION produced it — a proposal body is model output from one session,
  and a slot body is injected verbatim into a brief. With it OFF a nested slot
  path means nothing in particular — every
  slot goes into every brief, exactly as before the feature existed — so
  turning it back off makes personal slots visible to everyone again rather
  than stranding them.

  The `<user>` segment is whatever names you on this server — your username
  when there is one, otherwise the `sub` an OIDC-terminating ingress forwards,
  the same key that owns your handoffs and sessions. One consequence is worth
  stating: a subject that cannot be a path segment (an issuer URL, say) cannot
  own a namespace, so with the feature ON your slot writes are REFUSED rather
  than dropped onto the shared slot every other operator reads. If your ingress
  forwards subjects of that shape, forward a `preferred_username` beside it (see
  `X-Memory-Actor-User` above) before turning `[slots] per_user` on. A username
  always wins over a subject, so adding one later does not re-bucket the slots
  already written under it.

  One gap is deliberate and documented rather than closed: `ai-memory bootstrap`
  writes pages at paths the model picks from the repository's own README, docs
  and code, with no operator to attribute them to, so a repo carrying injected
  instructions can make it write a `_slots/…` page. It is an admin-only
  operation on a repository the admin chose to ingest, and the behaviour is the
  same with the flag off; review `bootstrap.md` — it lists every path written.
- **Auto-improvement proposals.** Each records the operator who staged it (the
  actor username, so proxy-asserted humans count too, and it shows up on the
  proposal detail and its `_pending/auto-improve/<id>.md` sidecar), and the "one
  pending proposal per page" rule applies per operator, so they stop blocking
  each other. Only where the deployment distinguishes operators, though:
  elsewhere proposals stay unattributed and the original one-per-page rule holds
  unchanged. A scheduled run has no caller, so it is attributed to the operator
  of the session it reviewed; the telemetry report and the curator describe the
  project rather than a person and stay unattributed, so they neither block nor
  are blocked by any named operator's pending proposal for the same page.

  That attribution is also what decides where a slot proposal goes with
  `[slots] per_user` on, and it has to be decided at staging: an approval is
  bound to the proposal's recorded target page and the snapshot taken of it at
  staging, so nothing can re-home it later. The reviewer is allowed to name
  `_slots/current-focus.md` and only that, so with the feature on the proposal is
  rewritten to `_slots/<staging operator>/current-focus.md` and approval accepts
  exactly that target — whoever approves, including the unattended scheduler,
  which is nobody. A slot proposal that cannot be attributed to an operator is
  refused rather than left on the shared slot: `_slots/current-focus.md` stays
  readable by everyone, but it is not a destination for one session's output.
  Refusals cost that one proposal — the run's others still apply — and are
  reported in the same `skipped` list as a staging collision.
- **Page reinforcement.** Reads are counted per operator as well as in the
  shared counter. `[decay] breadth_weight` (default `0.0`) optionally lets a
  page reinforced by many different people outrank one read repeatedly by a
  single person — the forget sweep reads the per-page count of distinct
  operators and feeds it into the retention score. At the default, and for pages
  with fewer than two distinct readers at any weight, retention scores are
  unchanged.

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
