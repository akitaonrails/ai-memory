# Design proposal: per-project authorization for multi-user servers (#708)

**Implementation status.** Slices 2 and 3 have landed. Slice 2 is the inert
V68 schema (`project_grants` + `projects.access_mode`, default `open`) and the
`authorize_project` choke point (`ai-memory-store/src/project_authz.rs`),
wired into `ScopeResolver` read/write resolution and the writer actor. Slice 3
makes `restricted` safe end-to-end:

- **Creator.** V69 adds `projects.created_by`, set when a database user's call
  creates the project; `resolve_project_authz` derives `is_creator` from it.
  Projects that predate V69 have none, and admit only root and grants once
  restricted.
- **Every surface opts in.** MCP tools, `/api/v1`, the web pages, hook routes
  and captures attach `ScopeResolver::with_project_authz` (or its free-function
  form, `authorize_scope_for`) for every database user. Root and installs with
  no database users attach nothing and behave as before.
- **Read-shaped mutations need write.** Tools that take read-shaped arguments
  but mutate (delete, feedback, sweep, lint, auto-improve, handoff accept and
  cancel, message pop and cancel, a message's recipient) resolve at
  `ProjectAccess::Write`.
- **Unscoped reads** (search, listings, the graph and the workspace
  overview) are filtered in SQL before `LIMIT` with the same rule the choke
  point applies; the global preferences scope stays readable by everyone.
- **Raw-id entry points** (managed runs, workstreams, session-scoped
  consolidation) resolve the id to its project and authorize it.
- **Management surface**, root-only: `ai-memory project access`,
  `ai-memory user grant|revoke|grants`, `ai-memory project grants`, and the
  matching `/admin` routes; every change is recorded in `audit_log`.
  `[auth] new_projects_restricted` makes new projects start `restricted`.
- **Captures** into a project the author may not write are dropped and counted
  (`dropped_unauthorized`), never retried.

Per-project administrators remain out of scope: granting, revoking and
restricting are root's alone. Sections below are the original design pass,
retained as the spec.

**Original status: proposal for review, not implemented.** This was the design
pass promised on #708 before any code lands. It changes a security boundary, so
it is deliberately separated from implementation.

## Problem

On a multi-user server (one where DB users exist, or a trusted identity proxy is
configured, so `deployment_distinguishes_operators()` is true), a DB-user token attributes
writes to that user but does **not** scope which projects the user may read or write.
Any authenticated non-root caller can resolve any `(workspace, project)` and read or
write it. Attribution exists; authorization does not. `/admin/*` is already root-only,
and `OwnerFilter` isolates *handoffs* per operator, but pages/observations/search are
project-scoped only, never user-scoped.

This is fine for the default posture (loopback, single operator) and is why it has not
bitten single-user installs. It is a real gap for a shared server hosting several teams.

## Current model (what exists today)

- **Auth ladder** (`ai-memory-core::actor`): `AuthLevel` = `Anonymous` / `User` / `Root`;
  `Capability` = `Admin` / `UserManagement` / `NormalRead` / `NormalWrite` /
  `SkipAdmissionChain`. `authorize(capability, distinguishes_operators)` gates them.
  `NormalRead`/`NormalWrite` are currently granted to everyone (no project dimension).
- **Identity**: `IdentityKey` (storage-key form `user:alice`, `oidc:…`); `ActorContext`
  carries the resolved user. DB users live in the `users` table (token-hash auth).
- **Scope**: `ScopeResolver` resolves `(workspace_id, project_id)`; reads use no-create
  lookups and fail closed on missing scope. `OwnerFilter` (pages shared, batons owned)
  applies to handoffs only; invariant #16 forbids it becoming a page-read filter.

## Proposed model

Introduce an explicit, additive **project grant** keyed by `(user, workspace, project,
level)`. Deny-by-default only when a project has *declared* itself access-controlled,
so existing open deployments do not silently lock out on upgrade.

### Grant levels
`read` (query/read pages/observations/status/briefing in that project) and `write`
(above + write_page/consolidate/handoff/message/delete). `admin` stays global/root as
today (project-admin is out of scope for v1).

### Schema (new migration, next free V on `release/2.5`)
```sql
CREATE TABLE project_grants (
    workspace_id BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id   BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    user_id      BLOB NOT NULL REFERENCES users(id)      ON DELETE CASCADE,
    level        TEXT NOT NULL CHECK (level IN ('read','write')),
    granted_by   BLOB REFERENCES users(id) ON DELETE SET NULL,
    granted_at   INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, project_id, user_id)
) WITHOUT ROWID;
```
A per-project flag decides enforcement: a project row gains
`access_mode TEXT CHECK (access_mode IN ('open','restricted')) DEFAULT 'open'`. `open`
= today's behavior (any authenticated user). `restricted` = only root, the project
creator, and users with a matching grant.

### Enforcement points (fail closed)
A single choke point, `authorize_project(actor, ws, proj, need: read|write)`, consulted by:
- `ScopeResolver` read/write resolution (the one place every MCP/API/web path already
  funnels through), and
- the writer actor as defense in depth for destructive ops.

`open` project → allow (current behavior). `restricted` → allow root; allow a `write`
grant for writes and a `read`/`write` grant for reads; else `AuthzError::Forbidden`.
Anonymous is denied on any restricted project. This must **not** reintroduce the
invariant-#16 hazard: the grant gate is an *authorization* check that returns
allow/deny; it is not an `OwnerFilter` on page rows. A team with grants still sees the
same shared pages: grants gate entry to the project, not row visibility within it.

### Enforcement gaps beyond the choke point (must close before enforcing)

The single `authorize_project` choke point at scope resolution is **necessary but not
sufficient**: two read classes never resolve a single `(workspace, project)` scope, so
they bypass the gate entirely. Both must be handled or a `restricted` project leaks.

1. **Unscoped / cross-project reads.** `memory_query(global=true)`, global `memory_recent`,
   and cross-project web search fan out across every project and never call the scope
   resolver, so `authorize_project` is never reached. These must filter the **result set
   by the caller's readable projects inside SQL, before `LIMIT`**, not after. Post-filtering
   a materialized page would still leak the *existence and count* of hits in projects the
   caller cannot read (and could starve the visible results under the limit). Concretely:
   join candidate rows against `project_grants` (+ `access_mode='open'` + creator + root)
   and apply the predicate in the same query that orders and limits. A caller with no
   grants sees exactly the open projects, as today.
2. **Raw-id entry points.** Several paths take a `session_id` / `run_id` / `page_id`
   directly and resolve their project *from the row*, bypassing scope resolution by
   construction: consolidation-by-session-id, managed-run routes, SessionStart handoff
   delivery, and `ReaderPool::page_evidence_counts(page_ids)`. Each must resolve the id →
   its `(workspace, project)` and then call `authorize_project` before returning content;
   an unauthorized id is `NotFound`/`Forbidden`, never a silent read. Audit every
   entry point that accepts a bare id against this rule; a new one that skips it silently
   reopens the hole while every scoped test still passes.

Structural enforcement (recommended): make the unguarded lookups crate-private so the
compiler forces new call sites through the guarded resolver + `authorize_project`, the
same way `ScopeResolver` already funnels scoped access.

### Ship inert, and never fail closed on missing rows

The feature must be a no-op until an operator opts in, and it must not turn a resolution
gap into a lockout (the #678 dead-end lesson: an unresolved pointer means degrade, not
refuse). Concretely: an empty `project_grants` table plus `access_mode DEFAULT 'open'`
means every existing project stays open; enabling `restricted` on a project with **zero
grants still admits root and the project creator** (never "locked out of my own data");
a startup that cannot read the grants table (older schema mid-migration) degrades to
`open` with a loud warning rather than denying every read; and single-user / loopback
(`distinguishes_operators()` false) skips the gate entirely. Enforcement is gated on
both the server distinguishing operators **and** the specific project being `restricted`.

### Management surface (root-only, `/admin/*`)
`ai-memory user grant --user alice --workspace w --project p --level write` and
`… revoke …`, plus `ai-memory project access --workspace w --project p --mode restricted|open`.
REST under `/admin/projects/*` and `/admin/users/*`, mirroring existing admin routes.

### Migration / rollout
Additive: every existing project defaults to `access_mode='open'`, so nothing changes
for current deployments on upgrade. An operator opts a project into `restricted`
explicitly, then grants users. Single-user/loopback is unaffected (no DB users →
`distinguishes_operators()` false → gate is a no-op).

## Open questions for review
1. **Default for NEW projects on a multi-user server**: keep `open` (least surprise) or
   `restricted` to the creator (secure-by-default)? Proposal: `open`, with a server
   config `[auth] new_projects_restricted = true` to flip it, so secure-by-default is
   available without breaking the common case.
2. **Global scope (`_global`)**: read-open to all authenticated users (it is shared
   preference context), never restricted. Writing it needs root or an explicit
   `write` grant on it (GHSA-7qj3-7wqw-m5w6): an open write there reaches every
   user's reads, restricted projects included.
3. **Interaction with cross-project messaging (V64)**: a `restricted` recipient inbox
   should require the sender to hold a `write` grant on the recipient project, or the
   message is refused; otherwise grants are bypassable via the mailbox. This ties #708
   to the messaging feature and is why it must be designed, not bolted on.
4. **Handoff `OwnerFilter`**: unchanged; grants and owner-batons are orthogonal.

## Non-goals (v1)
Per-project *admin* delegation, row-level ACLs, time-boxed grants, group/role
abstractions. Those can layer on the `project_grants` table later.

## Verification plan (when approved)
Table-driven authorization tests (root / granted-read / granted-write / no-grant /
anonymous × open/restricted projects), a multi-session integration test proving a
non-granted user is refused a restricted project while a granted teammate is admitted
(the invariant-#16 shape that unit tests miss), and a migration idempotency test. Plus,
for the two bypass classes above:
- an **unscoped-read leak test**: `memory_query(global=true)` / global `recent` / web
  search by a caller with no grant returns hits only from open projects and never
  reveals the count or existence of hits in a `restricted` project (the filter is
  applied before `LIMIT`, verified with enough rows that a naive post-filter would
  under-fill the visible window);
- a **raw-id authz test** per bare-id entry point (session/run/page id, and
  `page_evidence_counts`): an id belonging to a `restricted` project the caller lacks a
  grant for returns `Forbidden`/`NotFound`, not content;
- a **ship-inert test**: an empty grants table, and a `restricted` project with zero
  grants, both still admit root and the creator; a caller on an `open` project is
  unaffected.

## Identity routing (slice 4, #925 — resolved: always-on)

Authorization is only sound if two unrelated checkouts that happen to share a
folder basename (`~/work/api` and `~/clients/acme/api`) resolve to **different**
projects. Otherwise one grant silently covers both. Slice 4 routes captures by
**repository identity** (the normalized git remote) instead of folder name:

- A new `V70` migration adds `projects.identity` / `identity_source`
  (`NOT NULL DEFAULT ''`) with a **partial** unique index
  `(workspace_id, identity) WHERE identity <> ''` and **no backfill** (a
  backfill on `lower(name)` would fail a workspace holding both `API` and `api`).
- Identity is derived by `git2::Repository::discover` (config read only) and
  normalized **lexically** (scheme split, credential strip, `.git`/slash tidy),
  never `fs::canonicalize`. Credentials in a remote URL are stripped client-side
  and never sent; the server re-validates any wire `identity` (`accept_wire_identity`).
- Resolution order is **explicit scope > declared `project` > git-remote identity
  > folder name**; a non-git directory falls back to the folder-name behavior
  (fail-closed, no new collision).
- The same normalization runs at all four capture front doors (native
  `ai-memory hook`, the shell bundle, the PowerShell bundle, and the generated
  TypeScript integrations), checked by a shared-fixture parity test so they
  cannot drift.

**Decision (resolved):** this is **always-on** in 2.5.0, not gated behind a
flag. Opt-in would leave the same-basename grant hole open for anyone who did
not opt in, defeating the authorization slices. The trade-off is that an upgrading
install's captures re-bucket by repository identity (two same-name repos split; one
repo opened from two folders converges); the CHANGELOG documents it.

## Path-keyed coordinates (#1033 — staged)

Identity routing already converges worktrees and clones of one repository into
one project for **captures**. Two gaps remain: the project's **name** is still
the first-seen folder basename, and MCP tools resolve `(workspace, project)` by
that name, so a static client that passes a worktree folder misses the merged
project. #1033 makes a remote-derived key the default name, landing in steps on
`release/2.6`:

1. **Normaliser and opt-in naming (landed first).** `repository_identity::
   styled_key` spells an identity under `IdentityStyle`: `path` drops a git
   remote's host (`github.com/acme/api` → `acme/api`); `host_path` is the #708
   identity unchanged. A marker's explicit style is forwarded by all four capture clients
   alongside a remote identity, and `resolve_project_by_identity` then names a
   project it **creates** `path_style_name(identity)` (`acme-api`: `/` written
   as `-`, today's split-name character rule). Captures keep routing by the
   full identity. **Cross-forge collisions are detected, never merged:** the
   path name is used only while no project in the workspace holds it, checked
   in the same transaction as the identity match, so `gitlab.com/acme/api`
   after `github.com/acme/api` falls back to the name it would otherwise get.
   Existing projects, whether matched, claimed or unclaimed, are never renamed.
   Checked against the `styled_key` and `identity_style` sections of
   `fixtures/remote_identity_cases.json` (core plus shell, PowerShell and
   TypeScript parity), store adversarial tests, and a two-operator
   `multi_session.rs` case. Reporting the fallback in `doctor` is a follow-up.
2. **Default naming chain (landed final).** `upstream` → `origin` supplies the
   full hostful identity and the canonical hostless path name for new projects;
   only a checkout with no valid network remote falls back to its folder. Marker
   `project`/`identity` and operator-home routes still outrank this inference.
   Phase-5 clients explicitly send `path`; explicit `host_path` is forwarded as
   the compatibility opt-out. A new server still treats omission as `host_path`
   because old clients omitted that historical default. Old servers safely
   accept or ignore the existing style field; no new wire field is required.
3. **Dual-key resolve and in-place name promotion (landed).** Static callers
   still pass only `workspace` + `project`. Within that workspace, exact stored
   name, the canonical path-style name derived from a stored full hostful git
   identity, and the v2 repository basename resolve to one UUID only when the
   candidate set is unique. Reads are no-create and never rename. An existing
   write path resolves, authorizes at `ProjectAccess::Write`, and promotes only
   `projects.name` in one writer transaction with its audit row; UUID foreign
   keys stay put. A target-name conflict, two forges sharing the hostless key,
   or multiple candidates fails closed. The legacy basename remains readable
   after promotion through v2. No alias table was added. V76 stores the two
    deterministic keys derived from V70's full `projects.identity` and indexes
    them so hot reads/writes use three bounded equality probes instead of
    scanning a workspace; startup backfills pre-V76 identity rows before traffic.
    Project `_meta.md` stores optional typed `identity` and `identity_source`
    plus the derived `canonical_name`/`legacy_name` keys. Clean
    reindex validates the pair, derives the indexed keys from the identity, and
    recreates the original UUID/name; pre-V76 manifests without identity remain
     valid and rebuild an identity-less project.
4. **Coordinate diagnostics (landed).** `GET /admin/project-coordinate` and
   `ai-memory doctor` classify exact, canonical-compatibility,
   legacy-compatibility, missing, and ambiguous coordinates. They report
   rename eligibility and collision reasons from read-only indexed store
   queries; no project, claim, rename, audit entry, or manifest is written.
   The admin route follows the existing single-user compatibility contract and
   becomes root-only whenever the deployment distinguishes operators. Stored
   remote identities are not returned; the CLI parses one typed local marker
   inspection, honours CLI-over-marker scope precedence, supplies only its
   normalized credential-free identity, and renders safe marker/source classes,
   source/style and derived names. Ambiguous server scope leaves local session
   evidence intact and marks captured counts unavailable rather than zero. An
   ambiguous result may carry the preferred identity-backed candidate UUID and
   current name for operator context; it still represents no unique resolution
   and is never rename-eligible.
5. **Local marker aliases (landed).** `aliases = ["former-name"]` is bounded,
   normalized, deduplicated and forwarded identically by native, POSIX shell,
   PowerShell and generated TypeScript clients on capture and handoff requests.
   The marker's `project` remains canonical. Alias lookup stays within its
   selected workspace and requires the checkout's full hostful git-remote
   identity to match the stored row; reads do not rename, while capture may use
   only the Phase 1 write-authorized canonical promotion. Alias lists are
   transient routing hints, not persisted rows, and stay supported after v3.
6. **Operator-home routes (landed).** The exact operator-home
   `.ai-memory.toml` accepts bounded namespaced identity and path tables. Exact
   normalized hostful identity wins the longest component-safe lexical path;
   local non-home settings markers win both. Route project names are canonical,
   route aliases reuse the Phase 3 wire only with remote identity plus marker
   provenance, malformed or ambiguous maps fail closed, and old line parsers
   ignore the deliberately prefixed `route_*` child fields. Native callers use
   `AI_MEMORY_HOME` first; portable clients use `HOME`/`USERPROFILE`, and Docker
    compares the forwarded host cwd.
7. **Default flip (landed final).** `Path` is now the explicit client default;
   the typed server/wire default remains legacy-safe `HostPath`. Authorized
   captures carrying explicit `path` can claim or promote compatible basename/
   host-path rows in the same writer transaction and preserve their UUID;
   handoff/static reads remain no-create/no-rename. Canonical names held by a
   different hostful identity fail closed and use the existing fallback. The
   indexed canonical/legacy compatibility keys and marker aliases remain through
   v3; no schema migration was required for this final slice.



Paths stay lexically normalised throughout; nothing canonicalises.
