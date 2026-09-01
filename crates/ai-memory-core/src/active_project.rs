//! A process-shared pointer to the project the user is currently active in.
//!
//! ## Why this exists (issue #2)
//!
//! The MCP protocol carries no working-directory context: a `memory_query`
//! call arrives with its arguments and nothing else, so a tool handler has
//! no way to know which project the agent is sitting in. The lifecycle hooks
//! *do* know — lifecycle `/hook` events carry the agent's `cwd`, and the hook
//! router resolves it to the correct per-cwd `(workspace_id, project_id)`.
//!
//! In HTTP mode the `/hook` ingress and the `/mcp` endpoint live in the same
//! process, so the hook router can publish "the project the user is currently
//! active in" to this shared pointer when work starts or advances, and the MCP
//! tools can read it as their default instead of falling back to the server's
//! static `--project` (which defaults to `scratch` and made the read tools
//! return empty memory even when the hooks were correctly populating a real
//! project). Completion events refresh exact actor-scoped entries only; a
//! delayed tail from an older session must not redirect a shared fallback.
//!
//! ## Isolation modes
//!
//! The default `Single` mode keeps the historical behaviour — one process-
//! wide slot, last-write-wins. That is right for a single operator running
//! one project at a time, but collapses parallel sessions on shared installs:
//! a hook firing from `~/repo-A` overwrites the slot a concurrent
//! `memory_query` (with no explicit project) in `~/repo-B` was about to read.
//!
//! Opt-in modes keep a per-key map alongside the single slot:
//!
//! - `PerSession` keys by `session_id` — isolates concurrent agent runs of
//!   the same operator (one person with several Claude Code / Codex windows
//!   in different repos at once).
//! - `PerActor` keys by `(user, session_id)` when both coordinates are present
//!   and also keeps a user-only slot for requests that truly have no session
//!   coordinate. That isolates across operators while preserving per-session
//!   isolation for clients/bridges that do forward one. Pairs with multi-user
//!   mode (rung 2): `user` comes from the `users` row that owns the bearer
//!   token.
//!
//! When the caller has no actor identity at all (anonymous request without a
//! session, or a code path the migration has not threaded yet), every mode
//! falls back to the single slot for backward compatibility. That is a legacy
//! convenience, not isolation. If a request provides a usable actor coordinate
//! and the keyed lookup misses, `get_for` returns `None` instead of falling
//! through to another session's latest project; callers then use their baked
//! default or an explicit workspace/project. The single slot is also what the
//! historical `set` / `get` / `clear` API touches, so existing callers compile
//! unchanged and admin ops like `move-project` still invalidate the pointer
//! correctly.
//!
//! ## Memory bound
//!
//! Per-key entries carry an [`Instant`] insertion timestamp and are evicted
//! on read or write once they exceed the configured TTL (default 1 hour).
//! A hard `max_entries` cap (default 4096) drops the oldest entries when
//! exceeded, so an adversarial / runaway client cannot grow the map without
//! bound. Both knobs are exposed via the CLI's `[auto_scope]` config block.
//!
//! ## Locking
//!
//! Locks are held only for the microseconds it takes to copy a small tuple
//! or do a single hash lookup; callers never `.await` while holding them, so
//! plain `std::sync::RwLock` is the right primitive (no async lock needed).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::ids::{ProjectId, WorkspaceId};

/// Default TTL for per-key entries in the opt-in isolation modes.
pub const DEFAULT_PER_KEY_TTL: Duration = Duration::from_secs(60 * 60);
/// Default upper bound on per-key entries, to keep memory finite on shared
/// installs where many short-lived sessions may come and go.
pub const DEFAULT_MAX_ENTRIES: usize = 4096;

/// Selects how the hook router and the MCP tools share the "currently active
/// project" pointer.
///
/// `PerActor` is the default: it keys the pointer by whatever coordinate the
/// caller actually has, so parallel harnesses and multiple operators stay
/// separated without configuration. A caller carrying no coordinate at all
/// still reads the shared slot, which is what keeps a single-harness install
/// behaving exactly as it always has.
///
/// `Single` remains available for installs that want the historical
/// process-wide slot explicitly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveProjectMode {
    /// Process-wide slot, last-write-wins. The historical behaviour, now opt-in.
    Single,
    /// Keyed by `session_id`. Isolates concurrent agent runs of the same
    /// operator from each other.
    PerSession,
    /// Keyed by `(user, session_id)`, with an identity-only slot alongside.
    /// Isolates parallel harnesses and separate operators. The default.
    #[default]
    PerActor,
}

/// Selects how the hook router attributes MID-SESSION events whose cwd has
/// moved since the session started. Set under `[routing] mid_session`.
///
/// Distinct from [`ActiveProjectMode`], which namespaces the in-process
/// "current project" pointer: this decides the DURABLE project written on
/// observations, and it never affects session-CREATING events — opening a
/// session always resolves from its own cwd.
///
/// `FollowCwd` is the default and preserves the historical behavior exactly:
/// every mid-session event re-resolves from its own cwd, so `cd`-ing into a
/// sibling checkout routes those observations to that checkout's project.
///
/// `Sticky` treats mid-session navigation as navigation: the session's project
/// stays the source of truth wherever the agent wanders. This matches the
/// product's own model — `sessions.project_id` holds exactly one value, and
/// consolidation writes one page in the session's project — so following the
/// cwd splits a session's raw record across projects while its page lands in
/// only one. A `.ai-memory.toml` marker still wins in both modes: naming a
/// project is a deliberate rescope, not drift (#394).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MidSessionRouting {
    /// Re-resolve every mid-session event from its own cwd. Historical
    /// behavior, and the default.
    #[default]
    #[serde(alias = "follow_cwd")]
    FollowCwd,
    /// Inherit the session's project for every mid-session event, overruling
    /// a host-derived repo-root override but never a marker-declared one.
    Sticky,
}

impl MidSessionRouting {
    /// Whether this mode lets an established session overrule a
    /// non-deliberate (host-derived) project override.
    #[must_use]
    pub const fn overrules_derived_override(self) -> bool {
        matches!(self, Self::Sticky)
    }

    /// Stable config/log representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FollowCwd => "follow-cwd",
            Self::Sticky => "sticky",
        }
    }
}

/// Composite identity used to key per-actor entries.
/// This cache key namespaces active-project pointers only; it does not
/// namespace durable hook `SessionId` records.
///
/// - `PerSession` mode populates only `session_id`.
/// - `PerActor` mode populates both (`user` holds the qualified identity key:
///   `user:<name>` or an issuer-qualified OIDC subject).
///
/// An [`ActorKey`] with both fields `None` is treated the same as "no actor"
/// — the request falls back to the single slot. That keeps anonymous /
/// pre-identity callers working without a special branch at every call site.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ActorKey {
    /// Qualified stable identity when the request was authenticated as a known
    /// user. The field name is retained for API compatibility; values are
    /// produced by `ActorContext::identity_key`, not copied from raw headers.
    pub user: Option<String>,
    /// Per-agent-run session identifier published by the lifecycle hooks
    /// (Claude Code, Codex, OpenCode, …). `None` when the call site has no
    /// session context (e.g. an ad-hoc MCP probe with no hook history).
    pub session_id: Option<String>,
}

impl ActorKey {
    /// `true` when neither identity coordinate is present — used to short-
    /// circuit the per-key map and fall back to the single slot.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.user.is_none() && self.session_id.is_none()
    }
}

/// One per-actor slot: the resolved project plus the actor's opted-in
/// `default_global` recall preference and the insertion time (for TTL).
#[derive(Debug, Clone, Copy)]
struct Entry {
    ws: WorkspaceId,
    proj: ProjectId,
    default_global: bool,
    inserted: Instant,
}

#[derive(Debug)]
struct PerActorMap {
    entries: HashMap<ActorKey, Entry>,
    ttl: Duration,
    max_entries: usize,
}

impl PerActorMap {
    fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            ttl: if ttl.is_zero() {
                DEFAULT_PER_KEY_TTL
            } else {
                ttl
            },
            max_entries: max_entries.max(1),
        }
    }

    /// Drop expired entries. Run on every read + write so a stale key never
    /// surfaces to a tool handler and the cap below sees an accurate size.
    fn purge_expired(&mut self, now: Instant) {
        self.entries
            .retain(|_, e| now.saturating_duration_since(e.inserted) < self.ttl);
    }

    /// Cap the map at `max_entries`, dropping the oldest insertions first.
    /// O(n log n) when over the cap; called only at insertion time, so the
    /// amortised cost is well below the hash insertion itself.
    fn enforce_cap(&mut self) {
        if self.entries.len() <= self.max_entries {
            return;
        }
        let excess = self.entries.len() - self.max_entries;
        let mut by_age: Vec<(ActorKey, Instant)> = self
            .entries
            .iter()
            .map(|(k, e)| (k.clone(), e.inserted))
            .collect();
        by_age.sort_by_key(|(_, inserted)| *inserted);
        for (k, _) in by_age.into_iter().take(excess) {
            self.entries.remove(&k);
        }
    }

    fn insert(
        &mut self,
        key: ActorKey,
        ws: WorkspaceId,
        proj: ProjectId,
        default_global: bool,
        now: Instant,
    ) {
        self.purge_expired(now);
        self.entries.insert(
            key,
            Entry {
                ws,
                proj,
                default_global,
                inserted: now,
            },
        );
        self.enforce_cap();
    }

    fn get(&mut self, key: &ActorKey, now: Instant) -> Option<(WorkspaceId, ProjectId)> {
        self.purge_expired(now);
        self.entries.get(key).map(|e| (e.ws, e.proj))
    }

    /// The actor's opted-in `default_global` preference, or `false` when no
    /// live entry exists for the key.
    fn get_default_global(&mut self, key: &ActorKey, now: Instant) -> bool {
        self.purge_expired(now);
        self.entries.get(key).is_some_and(|e| e.default_global)
    }

    fn retarget_project(&mut self, project_id: ProjectId, workspace_id: WorkspaceId, now: Instant) {
        self.purge_expired(now);
        for e in self.entries.values_mut() {
            if e.proj == project_id {
                e.ws = workspace_id;
            }
        }
    }

    fn clear_project(&mut self, project_id: ProjectId, now: Instant) {
        self.purge_expired(now);
        self.entries.retain(|_, e| e.proj != project_id);
    }

    fn clear_workspace(&mut self, workspace_id: WorkspaceId, now: Instant) {
        self.purge_expired(now);
        self.entries.retain(|_, e| e.ws != workspace_id);
    }

    fn contains_project(&mut self, project_id: ProjectId, now: Instant) -> bool {
        self.purge_expired(now);
        self.entries.values().any(|e| e.proj == project_id)
    }

    fn contains_workspace(&mut self, workspace_id: WorkspaceId, now: Instant) -> bool {
        self.purge_expired(now);
        self.entries.values().any(|e| e.ws == workspace_id)
    }

    /// Test-only: live row count in the backing HashMap. Counts all
    /// rows, including ones whose TTL has elapsed (those are dropped
    /// by `purge_expired` on the next read/write). The randomised cap
    /// test uses this to verify `enforce_cap` keeps the storage bounded
    /// at every step regardless of when expiry sweeps run.
    #[cfg(test)]
    fn raw_len(&self) -> usize {
        self.entries.len()
    }
}

/// Cheap, cloneable handle to the shared "currently active project" pointer.
///
/// Clones share the same underlying state. Starts empty; the hook router
/// fills it as events arrive.
#[derive(Clone)]
pub struct ActiveProject {
    mode: ActiveProjectMode,
    single: Arc<RwLock<Option<(WorkspaceId, ProjectId)>>>,
    /// `default_global` for the single/fallback slot (Single mode or an actor
    /// with no usable coordinate). Kept beside `single` rather than inside the
    /// tuple so the legacy `set`/`get` API and every existing caller stay
    /// byte-for-byte unchanged.
    single_default_global: Arc<RwLock<bool>>,
    per_actor: Arc<RwLock<PerActorMap>>,
    /// Whether any keyed entry has ever been published on this process.
    ///
    /// Distinguishes "this install has no lifecycle hooks, so nothing was ever
    /// keyed" from "hooks are running and this key simply does not match".
    /// The first deserves the shared fallback — there is no better information
    /// anywhere — while the second is a mismatch that must fail closed rather
    /// than answer for whichever project published last.
    ///
    /// Never reset: an install that once had hook activity is not hookless
    /// again just because entries aged out of the TTL map.
    ever_keyed: Arc<AtomicBool>,
}

impl Default for ActiveProject {
    fn default() -> Self {
        Self::with_config(
            ActiveProjectMode::default(),
            DEFAULT_PER_KEY_TTL,
            DEFAULT_MAX_ENTRIES,
        )
    }
}

impl ActiveProject {
    /// Create an empty pointer in the shipped default mode
    /// ([`ActiveProjectMode::PerActor`]).
    ///
    /// Was `Single` before v1.39. Call [`Self::with_mode`] explicitly for the
    /// historical process-wide slot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an empty pointer in the given mode, using the default TTL +
    /// cap for the per-key backing map.
    #[must_use]
    pub fn with_mode(mode: ActiveProjectMode) -> Self {
        Self::with_config(mode, DEFAULT_PER_KEY_TTL, DEFAULT_MAX_ENTRIES)
    }

    /// Full-fidelity constructor used by the CLI's `serve` wiring and by
    /// unit tests that need to exercise eviction.
    #[must_use]
    pub fn with_config(mode: ActiveProjectMode, ttl: Duration, max_entries: usize) -> Self {
        Self {
            mode,
            single: Arc::new(RwLock::new(None)),
            single_default_global: Arc::new(RwLock::new(false)),
            per_actor: Arc::new(RwLock::new(PerActorMap::new(ttl, max_entries))),
            ever_keyed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The mode this handle was built with. Mostly useful for tests and for
    /// observability (the `serve` startup log records it once).
    #[must_use]
    pub fn mode(&self) -> ActiveProjectMode {
        self.mode
    }

    /// Publish the project the agent is currently active in. Called by the
    /// hook router after it resolves an event's `cwd` to a real project.
    ///
    /// The actor's identity steers which slot is updated:
    /// - In `Single` mode (the default), the process-wide slot is overwritten.
    /// - In `PerSession` / `PerActor`, the entry keyed by the actor is set
    ///   *and* the single slot is updated as well, so callers that have no
    ///   actor identity (anonymous probes, legacy code paths) still see the
    ///   latest project rather than an empty pointer.
    /// - In `PerActor`, a user-only entry is also set when `user` is present.
    ///   Requests that carry that user but no session id can still resolve to
    ///   the user's latest project without falling through to the global slot.
    pub fn set_for(
        &self,
        actor: &ActorKey,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        default_global: bool,
    ) {
        self.write_single(workspace_id, project_id);
        self.write_single_default_global(default_global);
        if self.mode == ActiveProjectMode::Single || actor.is_empty() {
            return;
        }
        let scoped = self.scoped_key(actor);
        if scoped.is_empty() {
            return;
        }
        let mut guard = self.per_actor.write().unwrap_or_else(|e| e.into_inner());
        self.ever_keyed.store(true, Ordering::Relaxed);
        if self.mode == ActiveProjectMode::PerActor
            && actor.user.is_some()
            && actor.session_id.is_some()
        {
            guard.insert(
                ActorKey {
                    user: actor.user.clone(),
                    session_id: None,
                },
                workspace_id,
                project_id,
                default_global,
                Instant::now(),
            );
        }
        guard.insert(
            scoped,
            workspace_id,
            project_id,
            default_global,
            Instant::now(),
        );
    }

    /// Refresh only the exact actor-scoped entry without advancing either
    /// fallback slot.
    ///
    /// Hook completion events use this path so an exact `per_session` or
    /// `per_actor` caller keeps a fresh mapping, while a delayed tail from an
    /// older session cannot redirect the process-wide single slot or the
    /// identity-only fallback used by a caller with no session coordinate.
    /// `Single` mode and actors without a usable coordinate are no-ops.
    pub fn set_scoped_for(
        &self,
        actor: &ActorKey,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        default_global: bool,
    ) {
        if self.mode == ActiveProjectMode::Single || actor.is_empty() {
            return;
        }
        let scoped = self.scoped_key(actor);
        if scoped.is_empty() {
            return;
        }
        let mut guard = self.per_actor.write().unwrap_or_else(|e| e.into_inner());
        guard.insert(
            scoped,
            workspace_id,
            project_id,
            default_global,
            Instant::now(),
        );
    }

    /// Read the currently active project for the given actor, if any has
    /// been published yet. Falls back to the single slot only when:
    /// - the mode is `Single`, or
    /// - the actor is empty, or has no coordinate usable by the selected mode.
    ///
    /// When a request provides a usable session/user coordinate and that keyed
    /// entry is absent, return `None`. Falling through to the user/global latest
    /// slot would route a remote MCP request with a mismatched session id into
    /// whichever project published hook activity last.
    #[must_use]
    pub fn get_for(&self, actor: &ActorKey) -> Option<(WorkspaceId, ProjectId)> {
        if self.mode == ActiveProjectMode::Single || actor.is_empty() {
            return self.read_single();
        }

        let scoped = self.scoped_key(actor);
        if scoped.is_empty() {
            return self.read_single();
        }

        let now = Instant::now();
        let mut guard = self.per_actor.write().unwrap_or_else(|e| e.into_inner());
        if let Some(found) = guard.get(&scoped, now) {
            return Some(found);
        }

        // A keyed miss means one of two very different things.
        //
        // If this identity has published before — or anything on this process
        // has — then hooks are running and this coordinate simply does not
        // match one. That is a mismatch, and answering from the shared slot
        // would route the request into whichever project published last,
        // possibly another operator's. Fail closed and let the caller fall
        // back to its configured default.
        //
        // If nothing has *ever* been keyed, this install has no lifecycle
        // hooks feeding the pointer at all (MCP-only). There is no better
        // information anywhere, and the shared slot is exactly what such an
        // install has always used, so keep answering from it.
        if self.mode == ActiveProjectMode::PerActor && actor.user.is_some() {
            let identity_only = ActorKey {
                user: actor.user.clone(),
                session_id: None,
            };
            if guard.get(&identity_only, now).is_some() {
                return None;
            }
        }
        drop(guard);
        if self.ever_keyed.load(Ordering::Relaxed) {
            return None;
        }
        self.read_single()
    }

    /// Whether the actor opted this session into `default_global` recall (via
    /// the repo's `[recall] default_global` marker, published by the hook).
    /// Uses the SAME slot-selection as [`Self::get_for`], so the flag tracks
    /// the project pointer it was published alongside. Defaults to `false`
    /// (unchanged behaviour) for any actor/slot that never opted in.
    #[must_use]
    pub fn default_global_for(&self, actor: &ActorKey) -> bool {
        if self.mode == ActiveProjectMode::Single || actor.is_empty() {
            return self.read_single_default_global();
        }
        let scoped = self.scoped_key(actor);
        if scoped.is_empty() {
            return self.read_single_default_global();
        }
        let mut guard = self.per_actor.write().unwrap_or_else(|e| e.into_inner());
        guard.get_default_global(&scoped, Instant::now())
    }

    /// Project the actor's identity onto only the coordinates the current
    /// mode uses: `PerSession` keeps `session_id`, `PerActor` keeps both.
    fn scoped_key(&self, actor: &ActorKey) -> ActorKey {
        match self.mode {
            ActiveProjectMode::Single => ActorKey {
                user: None,
                session_id: None,
            },
            ActiveProjectMode::PerSession => ActorKey {
                user: None,
                session_id: actor.session_id.clone(),
            },
            ActiveProjectMode::PerActor => actor.clone(),
        }
    }

    fn write_single(&self, ws: WorkspaceId, proj: ProjectId) {
        let mut guard = self.single.write().unwrap_or_else(|e| e.into_inner());
        *guard = Some((ws, proj));
    }

    fn read_single(&self) -> Option<(WorkspaceId, ProjectId)> {
        let guard = self.single.read().unwrap_or_else(|e| e.into_inner());
        *guard
    }

    fn write_single_default_global(&self, value: bool) {
        let mut guard = self
            .single_default_global
            .write()
            .unwrap_or_else(|e| e.into_inner());
        *guard = value;
    }

    fn read_single_default_global(&self) -> bool {
        let guard = self
            .single_default_global
            .read()
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }

    /// Legacy single-slot setter — used by tests and by call sites that
    /// have no actor context yet. Touches the single slot only; the per-key
    /// map is untouched.
    pub fn set(&self, workspace_id: WorkspaceId, project_id: ProjectId) {
        self.write_single(workspace_id, project_id);
    }

    /// Legacy single-slot getter — same contract as [`Self::set`].
    #[must_use]
    pub fn get(&self) -> Option<(WorkspaceId, ProjectId)> {
        self.read_single()
    }

    /// Forget the active project. Called after an admin operation invalidates
    /// the published pointer (e.g. a `move-project` whose copy-purge path
    /// gives the project a NEW id, so the old pointer no longer resolves).
    /// Clears both the single slot AND the per-key map, since the project
    /// id is gone for every caller.
    pub fn clear(&self) {
        {
            let mut guard = self.single.write().unwrap_or_else(|e| e.into_inner());
            *guard = None;
        }
        let mut guard = self.per_actor.write().unwrap_or_else(|e| e.into_inner());
        guard.entries.clear();
    }

    /// Retarget every active entry for `project_id` to `workspace_id`, preserving
    /// the project id. Used after a lossless cross-workspace move where the
    /// project row survives and only its workspace changes.
    pub fn retarget_project_workspace(&self, project_id: ProjectId, workspace_id: WorkspaceId) {
        {
            let mut guard = self.single.write().unwrap_or_else(|e| e.into_inner());
            if let Some((ws, proj)) = guard.as_mut()
                && *proj == project_id
            {
                *ws = workspace_id;
            }
        }
        let mut guard = self.per_actor.write().unwrap_or_else(|e| e.into_inner());
        guard.retarget_project(project_id, workspace_id, Instant::now());
    }

    /// Clear every active entry whose project id matches `project_id`.
    pub fn clear_project(&self, project_id: ProjectId) {
        {
            let mut guard = self.single.write().unwrap_or_else(|e| e.into_inner());
            if guard.map(|(_, proj)| proj) == Some(project_id) {
                *guard = None;
            }
        }
        let mut guard = self.per_actor.write().unwrap_or_else(|e| e.into_inner());
        guard.clear_project(project_id, Instant::now());
    }

    /// Clear every active entry whose workspace id matches `workspace_id`.
    pub fn clear_workspace(&self, workspace_id: WorkspaceId) {
        {
            let mut guard = self.single.write().unwrap_or_else(|e| e.into_inner());
            if guard.map(|(ws, _)| ws) == Some(workspace_id) {
                *guard = None;
            }
        }
        let mut guard = self.per_actor.write().unwrap_or_else(|e| e.into_inner());
        guard.clear_workspace(workspace_id, Instant::now());
    }

    /// Whether any live active entry references `project_id`.
    #[must_use]
    pub fn contains_project(&self, project_id: ProjectId) -> bool {
        if self.read_single().map(|(_, proj)| proj) == Some(project_id) {
            return true;
        }
        let mut guard = self.per_actor.write().unwrap_or_else(|e| e.into_inner());
        guard.contains_project(project_id, Instant::now())
    }

    /// Whether any live active entry references `workspace_id`.
    #[must_use]
    pub fn contains_workspace(&self, workspace_id: WorkspaceId) -> bool {
        if self.read_single().map(|(ws, _)| ws) == Some(workspace_id) {
            return true;
        }
        let mut guard = self.per_actor.write().unwrap_or_else(|e| e.into_inner());
        guard.contains_workspace(workspace_id, Instant::now())
    }

    /// Test-only: look up only the per-key backing store. Used to prove
    /// eviction in tests without depending on the public fallback semantics.
    #[cfg(test)]
    fn keyed_only_get(&self, actor: &ActorKey) -> Option<(WorkspaceId, ProjectId)> {
        if self.mode == ActiveProjectMode::Single || actor.is_empty() {
            return None;
        }
        let scoped = self.scoped_key(actor);
        if scoped.is_empty() {
            return None;
        }
        let mut guard = self.per_actor.write().unwrap_or_else(|e| e.into_inner());
        guard.get(&scoped, Instant::now())
    }

    /// Test-only: live count of keyed entries in the per-actor backing
    /// store, after any pending TTL expiry. Used by the randomised
    /// invariant tests to pin `count <= max_entries` after every
    /// operation. Counts both still-fresh and stale entries because
    /// pre-expiry sweeping is a `get`-time concern; this helper
    /// inspects raw storage so the cap test catches a cap violation
    /// regardless of when expiry sweeps run.
    #[cfg(test)]
    fn keyed_entry_count_for_test(&self) -> usize {
        let guard = self.per_actor.read().unwrap_or_else(|e| e.into_inner());
        guard.raw_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_session(s: &str) -> ActorKey {
        ActorKey {
            user: None,
            session_id: Some(s.to_string()),
        }
    }

    fn key_actor(user: &str, session: &str) -> ActorKey {
        ActorKey {
            user: Some(user.to_string()),
            session_id: Some(session.to_string()),
        }
    }

    fn empty_actor() -> ActorKey {
        ActorKey {
            user: None,
            session_id: None,
        }
    }

    fn per_actor() -> ActiveProject {
        ActiveProject::with_config(
            ActiveProjectMode::PerActor,
            DEFAULT_PER_KEY_TTL,
            DEFAULT_MAX_ENTRIES,
        )
    }

    fn ids(_n: u8) -> (WorkspaceId, ProjectId) {
        (WorkspaceId::new(), ProjectId::new())
    }

    /// The scenario the default must not break: one operator, one harness, and
    /// an MCP-only install with no lifecycle hooks feeding the pointer.
    ///
    /// Nothing is ever keyed, so a request carrying a session id has no keyed
    /// entry to match. That is not a mismatch — there is no better information
    /// anywhere — so it must keep reading the shared slot, exactly as it did
    /// before `PerActor` became the default.
    #[test]
    fn a_hookless_install_still_reads_the_shared_slot() {
        let ap = per_actor();
        let (ws, proj) = ids(1);
        ap.set(ws, proj);

        assert_eq!(
            ap.get_for(&key_actor("alice", "session-never-published")),
            Some((ws, proj)),
            "with no keyed activity anywhere, the shared slot is the only answer"
        );
    }

    /// Once hooks *are* publishing, a session id that matches nothing is a
    /// mismatch rather than a hookless install. Answering from the shared slot
    /// would hand back whichever project published last — on a shared server,
    /// somebody else's. Fail closed instead.
    #[test]
    fn a_session_mismatch_fails_closed_once_anything_has_been_keyed() {
        let ap = per_actor();
        let (ws_a, proj_a) = ids(1);
        ap.set_for(&key_actor("alice", "s-alice"), ws_a, proj_a, false);

        assert_eq!(
            ap.get_for(&key_actor("alice", "some-other-session")),
            None,
            "alice has published, so an unmatched session of hers is a mismatch"
        );
        assert_eq!(
            ap.get_for(&key_actor("carol", "s-carol")),
            None,
            "and another operator must not inherit alice's pointer"
        );
    }

    /// Scenario: one user, two harnesses open in the same project at once.
    /// Each must keep its own pointer; neither may overwrite the other.
    #[test]
    fn parallel_harnesses_of_one_user_keep_separate_pointers() {
        let ap = per_actor();
        let (ws_1, proj_1) = ids(1);
        let (ws_2, proj_2) = ids(2);

        ap.set_for(&key_actor("alice", "harness-a"), ws_1, proj_1, false);
        ap.set_for(&key_actor("alice", "harness-b"), ws_2, proj_2, false);

        assert_eq!(
            ap.get_for(&key_actor("alice", "harness-a")),
            Some((ws_1, proj_1))
        );
        assert_eq!(
            ap.get_for(&key_actor("alice", "harness-b")),
            Some((ws_2, proj_2)),
            "the second harness must not have clobbered the first"
        );
    }

    /// Scenario: two operators on one server. Neither may read the other's
    /// pointer, in either direction.
    #[test]
    fn two_operators_never_read_each_others_pointer() {
        let ap = per_actor();
        let (ws_a, proj_a) = ids(1);
        let (ws_c, proj_c) = ids(2);

        ap.set_for(&key_actor("alice", "s-a"), ws_a, proj_a, false);
        ap.set_for(&key_actor("carol", "s-c"), ws_c, proj_c, false);

        assert_eq!(ap.get_for(&key_actor("alice", "s-a")), Some((ws_a, proj_a)));
        assert_eq!(ap.get_for(&key_actor("carol", "s-c")), Some((ws_c, proj_c)));
    }

    /// The commonest authenticated upgrade path: lifecycle hooks publish with
    /// both a user and a session, while a *static* MCP client (no session-aware
    /// bridge) sends only its bearer.
    ///
    /// That client keys to the identity-only slot, which `set_for` publishes
    /// alongside the session slot precisely so this works. Without it, every
    /// static MCP client on an authenticated install would fail closed after
    /// the default changed — which would be a broken upgrade, not a safer one.
    #[test]
    fn a_static_mcp_client_reads_the_identity_only_slot() {
        let ap = per_actor();
        let (ws, proj) = ids(1);
        ap.set_for(&key_actor("alice", "hook-session"), ws, proj, false);

        let static_client = ActorKey {
            user: Some("alice".to_string()),
            session_id: None,
        };
        assert_eq!(
            ap.get_for(&static_client),
            Some((ws, proj)),
            "a bearer with no session must still resolve to that user's project"
        );
    }

    /// A caller with no coordinate at all — an anonymous probe, or a client
    /// that cannot forward identity — still reads the shared slot. This is what
    /// keeps the single-harness install unchanged under the new default.
    #[test]
    fn an_actorless_caller_still_reads_the_shared_slot_after_keying() {
        let ap = per_actor();
        let (ws_a, proj_a) = ids(1);
        ap.set_for(&key_actor("alice", "s-a"), ws_a, proj_a, false);

        assert_eq!(
            ap.get_for(&empty_actor()),
            Some((ws_a, proj_a)),
            "an empty actor has no coordinate to mismatch on"
        );
    }

    #[test]
    fn starts_empty() {
        assert!(ActiveProject::new().get().is_none());
    }

    #[test]
    fn set_then_get_round_trips_legacy_api() {
        let ap = ActiveProject::new();
        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        ap.set(ws, proj);
        assert_eq!(ap.get(), Some((ws, proj)));
    }

    #[test]
    fn set_overwrites_previous_legacy_api() {
        let ap = ActiveProject::new();
        ap.set(WorkspaceId::new(), ProjectId::new());
        let ws2 = WorkspaceId::new();
        let proj2 = ProjectId::new();
        ap.set(ws2, proj2);
        assert_eq!(ap.get(), Some((ws2, proj2)));
    }

    #[test]
    fn clones_share_one_slot() {
        let a = ActiveProject::new();
        let b = a.clone();
        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        a.set(ws, proj);
        assert_eq!(b.get(), Some((ws, proj)), "clone must see the same slot");
    }

    #[test]
    fn clear_drops_single_and_per_actor() {
        let ap = ActiveProject::with_mode(ActiveProjectMode::PerActor);
        let alice = key_actor("alice", "sA");
        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        ap.set_for(&alice, ws, proj, false);
        assert_eq!(ap.get_for(&alice), Some((ws, proj)));

        ap.clear();
        assert!(ap.get_for(&alice).is_none(), "per-actor entry must be gone");
        assert!(ap.get().is_none(), "single slot must be gone");
    }

    #[test]
    fn retarget_project_workspace_updates_single_and_keyed_entries() {
        let ap = ActiveProject::with_mode(ActiveProjectMode::PerActor);
        let alice = key_actor("alice", "sA");
        let bob = key_actor("bob", "sB");
        let old_ws = WorkspaceId::new();
        let new_ws = WorkspaceId::new();
        let moved = ProjectId::new();
        let other = ProjectId::new();

        ap.set_for(&alice, old_ws, moved, false);
        ap.set_for(&bob, old_ws, other, false);
        ap.set(old_ws, moved);

        ap.retarget_project_workspace(moved, new_ws);

        assert_eq!(ap.get(), Some((new_ws, moved)));
        assert_eq!(ap.get_for(&alice), Some((new_ws, moved)));
        assert_eq!(ap.get_for(&bob), Some((old_ws, other)));
        assert!(ap.contains_project(moved));
        assert!(ap.contains_workspace(new_ws));
    }

    #[test]
    fn clear_project_drops_matching_single_and_keyed_entries_only() {
        let ap = ActiveProject::with_mode(ActiveProjectMode::PerActor);
        let alice = key_actor("alice", "sA");
        let bob = key_actor("bob", "sB");
        let ws = WorkspaceId::new();
        let doomed = ProjectId::new();
        let kept = ProjectId::new();

        ap.set_for(&alice, ws, doomed, false);
        ap.set_for(&bob, ws, kept, false);
        ap.set(ws, doomed);

        ap.clear_project(doomed);

        assert!(ap.get().is_none());
        assert!(ap.get_for(&alice).is_none());
        assert_eq!(ap.get_for(&bob), Some((ws, kept)));
        assert!(!ap.contains_project(doomed));
        assert!(ap.contains_project(kept));
    }

    #[test]
    fn clear_workspace_drops_matching_single_and_keyed_entries_only() {
        let ap = ActiveProject::with_mode(ActiveProjectMode::PerActor);
        let alice = key_actor("alice", "sA");
        let bob = key_actor("bob", "sB");
        let doomed_ws = WorkspaceId::new();
        let kept_ws = WorkspaceId::new();
        let p1 = ProjectId::new();
        let p2 = ProjectId::new();

        ap.set_for(&alice, doomed_ws, p1, false);
        ap.set_for(&bob, kept_ws, p2, false);
        ap.set(doomed_ws, p1);

        ap.clear_workspace(doomed_ws);

        assert!(ap.get().is_none());
        assert!(ap.get_for(&alice).is_none());
        assert_eq!(ap.get_for(&bob), Some((kept_ws, p2)));
        assert!(!ap.contains_workspace(doomed_ws));
        assert!(ap.contains_workspace(kept_ws));
    }

    #[test]
    fn single_mode_ignores_actor_coordinates() {
        // Explicit: `Single` is opt-in since v1.39, so this pins the mode
        // rather than relying on whatever the default happens to be.
        let ap = ActiveProject::with_mode(ActiveProjectMode::Single);
        let alice = key_actor("alice", "sA");
        let bob = key_actor("bob", "sB");
        let ws = WorkspaceId::new();
        let p_alice = ProjectId::new();
        let p_bob = ProjectId::new();
        ap.set_for(&alice, ws, p_alice, false);
        ap.set_for(&bob, ws, p_bob, false);
        // Both reads see the last write; that's the legacy contract.
        assert_eq!(ap.get_for(&alice), Some((ws, p_bob)));
        assert_eq!(ap.get_for(&bob), Some((ws, p_bob)));
        assert_eq!(ap.get(), Some((ws, p_bob)));
    }

    #[test]
    fn per_session_isolates_by_session_id() {
        let ap = ActiveProject::with_mode(ActiveProjectMode::PerSession);
        let sess_a = key_session("sA");
        let sess_b = key_session("sB");
        let ws = WorkspaceId::new();
        let p_a = ProjectId::new();
        let p_b = ProjectId::new();
        ap.set_for(&sess_a, ws, p_a, false);
        ap.set_for(&sess_b, ws, p_b, false);
        assert_eq!(ap.get_for(&sess_a), Some((ws, p_a)));
        assert_eq!(ap.get_for(&sess_b), Some((ws, p_b)));
    }

    #[test]
    fn scoped_refresh_does_not_advance_single_fallback() {
        let ap = ActiveProject::with_mode(ActiveProjectMode::PerSession);
        let ws = WorkspaceId::new();
        let foreground = key_session("foreground");
        let stale = key_session("stale");
        let foreground_project = ProjectId::new();
        let stale_project = ProjectId::new();

        ap.set_for(&foreground, ws, foreground_project, true);
        ap.set_scoped_for(&stale, ws, stale_project, false);

        assert_eq!(ap.get(), Some((ws, foreground_project)));
        assert_eq!(ap.get_for(&foreground), Some((ws, foreground_project)));
        assert_eq!(ap.get_for(&stale), Some((ws, stale_project)));
        assert!(ap.default_global_for(&foreground));
        assert!(!ap.default_global_for(&stale));
    }

    #[test]
    fn scoped_refresh_does_not_advance_per_actor_identity_fallback() {
        let ap = ActiveProject::with_mode(ActiveProjectMode::PerActor);
        let ws = WorkspaceId::new();
        let foreground = key_actor("alice", "foreground");
        let stale = key_actor("alice", "stale");
        let foreground_project = ProjectId::new();
        let stale_project = ProjectId::new();

        ap.set_for(&foreground, ws, foreground_project, true);
        ap.set_scoped_for(&stale, ws, stale_project, false);

        let identity_only = ActorKey {
            user: Some("alice".to_string()),
            session_id: None,
        };
        assert_eq!(ap.get(), Some((ws, foreground_project)));
        assert_eq!(ap.get_for(&identity_only), Some((ws, foreground_project)));
        assert_eq!(ap.get_for(&stale), Some((ws, stale_project)));
        assert!(ap.default_global_for(&identity_only));
        assert!(!ap.default_global_for(&stale));
    }

    #[test]
    fn per_session_ignores_user_field() {
        let ap = ActiveProject::with_mode(ActiveProjectMode::PerSession);
        let ws = WorkspaceId::new();
        let p = ProjectId::new();
        let alice = key_actor("alice", "shared-session");
        let bob = key_actor("bob", "shared-session");
        ap.set_for(&alice, ws, p, false);
        let p_bob = ProjectId::new();
        ap.set_for(&bob, ws, p_bob, false);
        // Same session_id, different users → still collapses to one entry
        // (intentional: per_session is the right mode for single-operator,
        // multi-cwd installs; per_actor is the mode for multi-operator).
        assert_eq!(ap.get_for(&alice), Some((ws, p_bob)));
    }

    #[test]
    fn per_actor_isolates_by_user_and_session() {
        let ap = ActiveProject::with_mode(ActiveProjectMode::PerActor);
        let alice_a = key_actor("alice", "sA");
        let alice_b = key_actor("alice", "sB");
        let bob_a = key_actor("bob", "sA");
        let ws = WorkspaceId::new();
        let p1 = ProjectId::new();
        let p2 = ProjectId::new();
        let p3 = ProjectId::new();
        ap.set_for(&alice_a, ws, p1, false);
        ap.set_for(&alice_b, ws, p2, false);
        ap.set_for(&bob_a, ws, p3, false);
        assert_eq!(ap.get_for(&alice_a), Some((ws, p1)));
        assert_eq!(ap.get_for(&alice_b), Some((ws, p2)));
        assert_eq!(ap.get_for(&bob_a), Some((ws, p3)));
    }

    #[test]
    fn per_actor_user_only_lookup_stays_with_that_user() {
        let ap = ActiveProject::with_mode(ActiveProjectMode::PerActor);
        let alice_a = key_actor("alice", "sA");
        let bob_a = key_actor("bob", "sA");
        let ws = WorkspaceId::new();
        let p_alice = ProjectId::new();
        let p_bob = ProjectId::new();
        ap.set_for(&alice_a, ws, p_alice, false);
        ap.set_for(&bob_a, ws, p_bob, false);

        assert_eq!(
            ap.get_for(&ActorKey {
                user: Some("alice".into()),
                session_id: None,
            }),
            Some((ws, p_alice)),
            "missing session id must not fall through to Bob's global slot"
        );
    }

    #[test]
    fn per_actor_unknown_session_does_not_fall_back_to_user_or_single_slot() {
        let ap = ActiveProject::with_mode(ActiveProjectMode::PerActor);
        let alice_a = key_actor("alice", "hook-session");
        let bob_a = key_actor("bob", "hook-session");
        let ws = WorkspaceId::new();
        let p_alice = ProjectId::new();
        let p_bob = ProjectId::new();
        ap.set_for(&alice_a, ws, p_alice, false);
        ap.set_for(&bob_a, ws, p_bob, false);

        assert_eq!(
            ap.get_for(&ActorKey {
                user: Some("alice".into()),
                session_id: Some("different-mcp-session".into()),
            }),
            None,
            "mismatched session id must not degrade to Alice's latest slot or the global slot"
        );
        assert_eq!(
            ap.get(),
            Some((ws, p_bob)),
            "legacy single slot remains populated for actorless callers only"
        );
    }

    #[test]
    fn per_actor_falls_back_to_single_when_actor_is_empty() {
        let ap = ActiveProject::with_mode(ActiveProjectMode::PerActor);
        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        ap.set(ws, proj);
        assert_eq!(ap.get_for(&empty_actor()), Some((ws, proj)));
    }

    #[test]
    fn per_actor_set_also_updates_single_slot() {
        // Legacy callers (anonymous probes, code paths the migration has not
        // touched yet) keep seeing a fresh pointer via `get()`.
        let ap = ActiveProject::with_mode(ActiveProjectMode::PerActor);
        let alice = key_actor("alice", "sA");
        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        ap.set_for(&alice, ws, proj, false);
        assert_eq!(ap.get(), Some((ws, proj)));
    }

    #[test]
    fn default_global_round_trips_per_actor_and_defaults_false() {
        let ap = ActiveProject::with_mode(ActiveProjectMode::PerActor);
        let alice = key_actor("alice", "sA");
        let bob = key_actor("bob", "sB");
        let ws = WorkspaceId::new();
        let (pa, pb) = (ProjectId::new(), ProjectId::new());

        ap.set_for(&alice, ws, pa, true); // alice opted in
        ap.set_for(&bob, ws, pb, false); // bob did not

        assert!(ap.default_global_for(&alice), "alice opted in");
        assert!(!ap.default_global_for(&bob), "bob did not");
        // The project-pointer resolution is unaffected by the recall flag.
        assert_eq!(ap.get_for(&alice), Some((ws, pa)));
        assert_eq!(ap.get_for(&bob), Some((ws, pb)));
        // An actor that never published defaults to false (unchanged behaviour).
        assert!(!ap.default_global_for(&key_actor("carol", "sC")));
    }

    #[test]
    fn default_global_single_slot_tracks_last_publish() {
        let ap = ActiveProject::with_mode(ActiveProjectMode::Single);
        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        ap.set_for(&empty_actor(), ws, proj, true);
        assert!(ap.default_global_for(&empty_actor()));
        // A later publish without the flag clears it — the pointer and its
        // preference move together.
        ap.set_for(&empty_actor(), ws, proj, false);
        assert!(!ap.default_global_for(&empty_actor()));
    }

    #[test]
    fn per_actor_max_entries_evicts_oldest() {
        let ap = ActiveProject::with_config(ActiveProjectMode::PerActor, DEFAULT_PER_KEY_TTL, 2);
        let ws = WorkspaceId::new();
        let p1 = ProjectId::new();
        let p2 = ProjectId::new();
        let p3 = ProjectId::new();
        let k1 = key_session("s1");
        let k2 = key_session("s2");
        let k3 = key_session("s3");
        ap.set_for(&k1, ws, p1, false);
        std::thread::sleep(Duration::from_millis(2));
        ap.set_for(&k2, ws, p2, false);
        std::thread::sleep(Duration::from_millis(2));
        ap.set_for(&k3, ws, p3, false);
        // Use the test-only keyed-only getter so the cap assertion targets
        // the backing map directly.
        assert!(ap.keyed_only_get(&k1).is_none(), "k1 must be evicted");
        assert_eq!(ap.keyed_only_get(&k2), Some((ws, p2)));
        assert_eq!(ap.keyed_only_get(&k3), Some((ws, p3)));
    }

    #[test]
    fn per_actor_ttl_expires_entries() {
        let ap = ActiveProject::with_config(
            ActiveProjectMode::PerActor,
            Duration::from_millis(20),
            DEFAULT_MAX_ENTRIES,
        );
        let k = key_session("s");
        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        ap.set_for(&k, ws, proj, false);
        assert_eq!(ap.get_for(&k), Some((ws, proj)));
        std::thread::sleep(Duration::from_millis(40));
        // The per-actor entry must be gone; identified callers with expired
        // keyed entries must not fall through to a stale latest-project slot.
        assert!(ap.get_for(&k).is_none());
    }

    // ────────────────────────────────────────────────────────────────────
    // Randomised invariant tests.
    //
    // Each `prop_*` test runs a long sequence of operations driven by a
    // tiny xorshift RNG with a fixed seed (printed on failure for
    // repro). We assert universal invariants that must hold AFTER every
    // operation — not just at one cherry-picked moment — so a bug in
    // eviction order or TTL expiry that the fixed scenarios miss
    // surfaces here.
    //
    // No external dep: workspace deliberately curates its dev-deps and
    // adding `proptest` for one module is more cost than signal. The
    // randomized fuzzing below covers the same ground for the
    // invariants we care about; if a future regression motivates true
    // shrinking, swap in proptest then.
    // ────────────────────────────────────────────────────────────────────

    /// Minimal xorshift64* — deterministic, ~4 cycles/step, no deps.
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            // Avoid the all-zero degenerate state.
            Self(if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            })
        }
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn pick<T: Copy>(&mut self, slice: &[T]) -> T {
            slice[(self.next() as usize) % slice.len()]
        }
    }

    /// One operation in the randomised harness.
    #[derive(Debug, Clone, Copy)]
    enum Op {
        Set(usize), // set_for(actor_pool[idx], ws, fresh proj)
        Get(usize), // get_for(actor_pool[idx])
        ClearAll,   // clear() — single + keyed
        SetSingle,  // set(ws, fresh proj) — legacy single-slot path
        GetSingle,  // get() — legacy single-slot path
    }

    fn run_seeded<F: FnMut(usize, &Op, &ActiveProject)>(
        ap: &ActiveProject,
        seed: u64,
        steps: usize,
        actor_pool_size: usize,
        ws: WorkspaceId,
        mut on_step: F,
    ) {
        let mut rng = Rng::new(seed);
        let actors: Vec<ActorKey> = (0..actor_pool_size)
            .map(|i| ActorKey {
                user: Some(format!("user-{i}")),
                session_id: Some(format!("sess-{i}")),
            })
            .collect();

        let op_table = [
            Op::Set(0),
            Op::Get(0),
            Op::ClearAll,
            Op::SetSingle,
            Op::GetSingle,
        ];

        for step in 0..steps {
            let mut op = rng.pick(&op_table);
            if let Op::Set(_) = op {
                op = Op::Set((rng.next() as usize) % actor_pool_size);
            } else if let Op::Get(_) = op {
                op = Op::Get((rng.next() as usize) % actor_pool_size);
            }

            match op {
                Op::Set(i) => {
                    let proj = ProjectId::new();
                    ap.set_for(&actors[i], ws, proj, false);
                }
                Op::Get(i) => {
                    let _ = ap.get_for(&actors[i]);
                }
                Op::ClearAll => ap.clear(),
                Op::SetSingle => {
                    let proj = ProjectId::new();
                    ap.set(ws, proj);
                }
                Op::GetSingle => {
                    let _ = ap.get();
                }
            }

            on_step(step, &op, ap);
        }
    }

    /// Property: at every moment, the count of `keyed` entries must be
    /// `<= max_entries`. The LRU evictor is the only thing keeping the
    /// map bounded; a race that lets it grow past the cap would surface
    /// here within a few thousand random ops.
    #[test]
    fn prop_keyed_entries_never_exceed_max() {
        let ws = WorkspaceId::new();
        for seed in [1, 42, 1337, 0xdead_beef, 0xfeed_face] {
            let cap = 8;
            let ap =
                ActiveProject::with_config(ActiveProjectMode::PerActor, DEFAULT_PER_KEY_TTL, cap);
            // Pool larger than cap so eviction is exercised continuously.
            run_seeded(&ap, seed, 2_000, cap * 4, ws, |step, op, ap| {
                let count = ap.keyed_entry_count_for_test();
                assert!(
                    count <= cap,
                    "seed={seed:#x} step={step}: cap violated, count={count} > {cap} after {op:?}"
                );
            });
        }
    }

    /// Property: an entry's projection through `set_for(k, _, p)` must
    /// be observable by `keyed_only_get(k) == Some(_, p)` until either
    /// (a) another `set_for(k, _, p')` overrides it, (b) eviction
    /// kicks it out (only possible if cap was exceeded between writes),
    /// or (c) the TTL passes.
    ///
    /// This is the "fresh write is durable" contract — the foundation
    /// every read tool depends on.
    #[test]
    fn prop_fresh_write_round_trips_until_overwrite_or_evict() {
        let ws = WorkspaceId::new();
        for seed in [1, 99, 12_345, 0xc0ffee_u64] {
            let cap = 16;
            let pool_size = cap * 2;
            let ap = ActiveProject::with_config(
                ActiveProjectMode::PerActor,
                Duration::from_secs(60), // long TTL — eviction stays cap-driven
                cap,
            );
            let mut rng = Rng::new(seed);
            let actors: Vec<ActorKey> = (0..pool_size)
                .map(|i| ActorKey {
                    user: Some(format!("user-{i}")),
                    session_id: Some(format!("sess-{i}")),
                })
                .collect();

            // Walk: each step picks an actor, writes a fresh project id,
            // immediately reads back, asserts the read sees THAT id.
            // Whether other actors' entries were evicted is fine; the
            // write we JUST did MUST be readable.
            for step in 0..1_000 {
                let i = (rng.next() as usize) % pool_size;
                let proj = ProjectId::new();
                ap.set_for(&actors[i], ws, proj, false);
                let got = ap.keyed_only_get(&actors[i]);
                assert_eq!(
                    got,
                    Some((ws, proj)),
                    "seed={seed:#x} step={step}: fresh write for actor {i} not readable; got {got:?}"
                );
            }
        }
    }

    /// Property: `clear()` must wipe BOTH the single slot and every
    /// keyed entry. Operators rely on this for tests / admin-driven
    /// reset; a partial clear would leak stale routing.
    #[test]
    fn prop_clear_wipes_all_slots() {
        let ws = WorkspaceId::new();
        for seed in [1_u64, 7, 314_159, 271_828, 161_803] {
            let ap =
                ActiveProject::with_config(ActiveProjectMode::PerActor, DEFAULT_PER_KEY_TTL, 64);
            run_seeded(&ap, seed, 500, 8, ws, |_step, _op, _ap| {});
            // After arbitrary ops, clear and assert emptiness from every
            // angle. Use a fresh actor key not seen in the run.
            ap.clear();
            assert!(
                ap.get().is_none(),
                "seed={seed:#x}: single slot not cleared"
            );
            assert!(
                ap.keyed_only_get(&ActorKey {
                    user: Some("post-clear-probe".into()),
                    session_id: Some("post-clear".into()),
                })
                .is_none(),
                "seed={seed:#x}: keyed lookup after clear must be None"
            );
            assert_eq!(
                ap.keyed_entry_count_for_test(),
                0,
                "seed={seed:#x}: keyed_entry_count after clear must be 0"
            );
        }
    }

    /// Property: TTL is monotonic in time — once an entry has expired
    /// (the per-key TTL has elapsed since its last `set_for`), no
    /// `get_for` lookup should ever see it again via the *keyed* path.
    #[test]
    fn prop_expired_entry_is_not_resurrected_via_keyed_path() {
        let ws = WorkspaceId::new();
        for seed in [1_u64, 11, 222, 3_333] {
            let ttl = Duration::from_millis(40);
            let ap = ActiveProject::with_config(ActiveProjectMode::PerActor, ttl, 32);
            let mut rng = Rng::new(seed);
            let actors: Vec<ActorKey> = (0..8)
                .map(|i| ActorKey {
                    user: Some(format!("u-{i}")),
                    session_id: Some(format!("s-{i}")),
                })
                .collect();

            // Pre-populate every actor; record the write time relative
            // to the run start.
            for actor in &actors {
                ap.set_for(actor, ws, ProjectId::new(), false);
            }

            // Wait for TTL plus a margin so every pre-populated entry
            // is logically expired.
            std::thread::sleep(ttl + Duration::from_millis(30));

            // Now hit each actor with `keyed_only_get`. None of them
            // should resurrect.
            for (idx, actor) in actors.iter().enumerate() {
                assert!(
                    ap.keyed_only_get(actor).is_none(),
                    "seed={seed:#x} idx={idx}: expired keyed entry resurrected"
                );
            }

            // Cross-check that fresh writes still work after expiry —
            // the map shouldn't go into an unrecoverable state.
            let i = (rng.next() as usize) % actors.len();
            let proj = ProjectId::new();
            ap.set_for(&actors[i], ws, proj, false);
            assert_eq!(
                ap.keyed_only_get(&actors[i]),
                Some((ws, proj)),
                "seed={seed:#x}: fresh post-expiry write must round-trip"
            );
        }
    }
}
