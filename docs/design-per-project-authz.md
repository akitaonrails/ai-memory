# Design proposal: per-project authorization for multi-user servers (#708)

**Implementation status.** Slice 2 has landed: the inert V68 schema
(`project_grants` + `projects.access_mode`, default `open`) and the
`authorize_project` choke point (`ai-memory-store/src/project_authz.rs`),
wired into `ScopeResolver` read/write resolution and the writer actor. It is
always-allow (no project is `restricted` yet), so there is no behaviour change.
**Slice 3 is pending** and is what makes `restricted` safe end-to-end: closing
the two bypass classes below (unscoped/global reads filtered before `LIMIT`;
raw-id entry points resolving through `authorize_project`), the root-only
management surface (setting `restricted`, issuing grants), and a
`projects.created_by` source for the creator check. Sections below are the
original design pass, retained as the spec.

**Original status: proposal for review — not implemented.** This was the design
pass promised on #708 before any code lands. It changes a security boundary, so
it is deliberately separated from implementation.

## Problem

On a multi-user server (one where DB users exist, or a trusted identity proxy is
configured — `deployment_distinguishes_operators()` is true), a DB-user token attributes
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
  applies to handoffs only — invariant #16 forbids it becoming a page-read filter.

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
same shared pages — grants gate entry to the project, not row visibility within it.

### Enforcement gaps beyond the choke point (must close before enforcing)

The single `authorize_project` choke point at scope resolution is **necessary but not
sufficient**: two read classes never resolve a single `(workspace, project)` scope, so
they bypass the gate entirely. Both must be handled or a `restricted` project leaks.

1. **Unscoped / cross-project reads.** `memory_query(global=true)`, global `memory_recent`,
   and cross-project web search fan out across every project and never call the scope
   resolver, so `authorize_project` is never reached. These must filter the **result set
   by the caller's readable projects inside SQL, before `LIMIT`** — not after. Post-filtering
   a materialized page would still leak the *existence and count* of hits in projects the
   caller cannot read (and could starve the visible results under the limit). Concretely:
   join candidate rows against `project_grants` (+ `access_mode='open'` + creator + root)
   and apply the predicate in the same query that orders and limits. A caller with no
   grants sees exactly the open projects, as today.
2. **Raw-id entry points.** Several paths take a `session_id` / `run_id` / `page_id`
   directly and resolve their project *from the row*, bypassing scope resolution by
   construction: consolidation-by-session-id, managed-run routes, SessionStart handoff
   delivery, and `ReaderPool::page_evidence_counts(page_ids)`. Each must resolve the id →
   its `(workspace, project)` and then call `authorize_project` before returning content —
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
   config `[auth] new_projects_restricted = true` to flip it — so secure-by-default is
   available without breaking the common case.
2. **Global scope (`_global`)**: read-open to all authenticated users (it is shared
   preference context), never restricted. Writes stay as today.
3. **Interaction with cross-project messaging (V64)**: a `restricted` recipient inbox
   should require the sender to hold a `write` grant on the recipient project, or the
   message is refused — otherwise grants are bypassable via the mailbox. This ties #708
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
