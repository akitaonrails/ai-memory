//! Shared workspace/project scope resolution.
//!
//! HTTP admin routes, MCP tools, and the read-only web API all need the same
//! boundary rules: explicit read scopes fail closed, write scopes are the only
//! place that may create projects, and current-project defaults must respect the
//! actor-scoped active-project pointer. Keeping those policies here prevents
//! each surface from growing its own subtly different fallback chain.

use std::collections::HashSet;
use std::fmt;

use ai_memory_core::{
    ActiveProject, ActiveProjectLookup, ActorKey, ProjectId, ReadPointer, UserId, WorkspaceId,
};

use crate::error::StoreError;
use crate::project_authz::{GrantLevel, ProjectAccess, ProjectPrincipal};
use crate::{ReaderPool, WriterHandle};

/// Canonical error for partial explicit scope arguments.
pub const WORKSPACE_PROJECT_PAIR_REQUIRED: &str = "workspace and project must be provided together";

/// Message for [`ScopeResolutionError::AmbiguousUnscopedWrite`]. Names both fixes,
/// because the caller cannot tell from the failure alone which one applies to them.
pub const AMBIGUOUS_UNSCOPED_WRITE: &str = "cannot resolve a target for this write: the active-project pointer does not match this \
     caller. Pass an explicit workspace and project, or install the lifecycle hooks so the \
     pointer is populated";

/// Human-readable workspace/project pair supplied by an API caller.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScopeName {
    /// Workspace name.
    pub workspace: String,
    /// Project name within the workspace.
    pub project: String,
}

impl ScopeName {
    /// Build a scope name from any string-like values.
    #[must_use]
    pub fn new(workspace: impl Into<String>, project: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            project: project.into(),
        }
    }
}

/// Resolved database ids for a project scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResolvedScope {
    /// Owning workspace id.
    pub workspace_id: WorkspaceId,
    /// Project id inside the workspace.
    pub project_id: ProjectId,
}

impl ResolvedScope {
    /// Return the ids as the tuple used by existing reader/writer APIs.
    #[must_use]
    pub fn as_tuple(self) -> (WorkspaceId, ProjectId) {
        (self.workspace_id, self.project_id)
    }
}

/// Where a resolved read scope came from.
///
/// An unscoped read never fails: it degrades through the pointer, the startup
/// seed and the server default. That keeps reads working, and it also means a
/// caller cannot tell a correct answer from one about a different project
/// (#757). Surfaces report this next to the scope they answered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScopeSource {
    /// The caller named the project (with or without its workspace).
    Explicit,
    /// The caller's own keyed active-project entry — its hook session.
    Session,
    /// The process-wide slot: whichever project published last. The answer
    /// for a caller with no coordinate, and for every caller in `single` mode.
    SharedSlot,
    /// The startup seed (#678): the most recently active project on disk,
    /// standing in until the first hook event after a restart.
    StartupSeed,
    /// The server default, because no pointer information exists at all.
    Default,
    /// The server default, because the caller's coordinate matched no hook
    /// session — a static MCP client whose transport session id is not a
    /// lifecycle-hook session id.
    DefaultAfterMismatch,
}

impl ScopeSource {
    /// Stable snake_case label for responses and logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeSource::Explicit => "explicit",
            ScopeSource::Session => "session",
            ScopeSource::SharedSlot => "shared_slot",
            ScopeSource::StartupSeed => "startup_seed",
            ScopeSource::Default => "default",
            ScopeSource::DefaultAfterMismatch => "default_after_mismatch",
        }
    }

    /// True when the scope is a guess standing in for a caller the server
    /// could not identify, rather than information about that caller. The
    /// shared slot and the plain default are how pointer-less installs have
    /// always worked, so they do not count.
    #[must_use]
    pub fn is_fallback(self) -> bool {
        matches!(
            self,
            ScopeSource::StartupSeed | ScopeSource::DefaultAfterMismatch
        )
    }

    /// True when the scope was inferred from a pointer or default rather than
    /// stated by the caller ([`ScopeSource::Explicit`]) or bound to the
    /// caller's own hook session ([`ScopeSource::Session`]).
    ///
    /// Broader than [`Self::is_fallback`] on purpose: it also covers
    /// [`ScopeSource::SharedSlot`] (whichever project published last). Two
    /// same-operator agents with no session id share that one slot, so a
    /// no-scope read can resolve to a *different* project than the caller
    /// meant — the empty-pop dead-end where the on-start inbox notice counted
    /// one project's mail but a later no-scope `memory_message_pop` resolved
    /// another project's (empty) inbox and returned nothing. A surface that
    /// answers from an inferred scope should say so when the answer is empty.
    #[must_use]
    pub fn is_inferred(self) -> bool {
        !matches!(self, ScopeSource::Explicit | ScopeSource::Session)
    }
}

impl fmt::Display for ScopeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Scope-resolution failure, independent of HTTP/MCP response types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeResolutionError {
    /// Only one of workspace/project was provided.
    WorkspaceProjectPairRequired,
    /// A multi-scope entry had an empty workspace.
    ScopeWorkspaceEmpty,
    /// A multi-scope entry had an empty project.
    ScopeProjectEmpty,
    /// The caller supplied more scopes than the surface allows.
    TooManyScopes {
        /// Maximum number of scopes allowed by the caller surface.
        max: usize,
        /// Number of scopes the request supplied.
        actual: usize,
    },
    /// A workspace name did not resolve.
    WorkspaceNotFound {
        /// Workspace name supplied by the caller.
        workspace: String,
    },
    /// A project name did not resolve inside the provided workspace.
    ProjectNotFoundInWorkspace {
        /// Workspace name supplied by the caller.
        workspace: String,
        /// Project name supplied by the caller.
        project: String,
    },
    /// A project-only read did not resolve in either the actor's active
    /// workspace or the server's default workspace.
    ProjectNotFoundInActiveOrDefault {
        /// Project name supplied by the caller.
        project: String,
    },
    /// An unscoped write from a caller whose active-project pointer did not
    /// resolve. Writing it to the server default would silently misfile it.
    AmbiguousUnscopedWrite,
    /// A write-create policy was requested without a writer handle.
    WriterRequired,
    /// The per-project authorization choke point (#708) refused the caller a
    /// `restricted` project. Carries the policy message.
    Forbidden(String),
    /// Underlying store failure.
    Store(String),
}

impl ScopeResolutionError {
    /// True when the error is caused by malformed caller input rather than a
    /// missing object or an internal store failure.
    #[must_use]
    pub fn is_bad_request(&self) -> bool {
        matches!(
            self,
            ScopeResolutionError::WorkspaceProjectPairRequired
                | ScopeResolutionError::ScopeWorkspaceEmpty
                | ScopeResolutionError::ScopeProjectEmpty
                | ScopeResolutionError::TooManyScopes { .. }
                | ScopeResolutionError::AmbiguousUnscopedWrite
        )
    }

    /// True when the caller named a scope that does not exist.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            ScopeResolutionError::WorkspaceNotFound { .. }
                | ScopeResolutionError::ProjectNotFoundInWorkspace { .. }
                | ScopeResolutionError::ProjectNotFoundInActiveOrDefault { .. }
        )
    }

    /// True when the per-project authorization choke point (#708) refused the
    /// caller. Surfaces map this to a 403/permission error.
    #[must_use]
    pub fn is_forbidden(&self) -> bool {
        matches!(self, ScopeResolutionError::Forbidden(_))
    }
}

impl fmt::Display for ScopeResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScopeResolutionError::WorkspaceProjectPairRequired => {
                f.write_str(WORKSPACE_PROJECT_PAIR_REQUIRED)
            }
            ScopeResolutionError::ScopeWorkspaceEmpty => {
                f.write_str("scope workspace cannot be empty")
            }
            ScopeResolutionError::ScopeProjectEmpty => f.write_str("scope project cannot be empty"),
            ScopeResolutionError::AmbiguousUnscopedWrite => f.write_str(AMBIGUOUS_UNSCOPED_WRITE),
            ScopeResolutionError::TooManyScopes { max, .. } => {
                write!(f, "at most {max} scopes are allowed")
            }
            ScopeResolutionError::WorkspaceNotFound { workspace } => {
                write!(f, "workspace '{workspace}' not found")
            }
            ScopeResolutionError::ProjectNotFoundInWorkspace { workspace, project } => {
                write!(
                    f,
                    "project '{project}' not found in workspace '{workspace}'"
                )
            }
            ScopeResolutionError::ProjectNotFoundInActiveOrDefault { project } => {
                write!(
                    f,
                    "project '{project}' not found in the active or default workspace"
                )
            }
            ScopeResolutionError::WriterRequired => {
                f.write_str("scope resolver requires a writer for create-on-write resolution")
            }
            ScopeResolutionError::Forbidden(msg) => f.write_str(msg),
            ScopeResolutionError::Store(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ScopeResolutionError {}

impl From<StoreError> for ScopeResolutionError {
    fn from(value: StoreError) -> Self {
        ScopeResolutionError::Store(value.to_string())
    }
}

/// Resolves workspace/project names according to the policy requested by the
/// caller. Construct per request; it only borrows existing handles.
pub struct ScopeResolver<'a> {
    reader: &'a ReaderPool,
    writer: Option<&'a WriterHandle>,
    active_project: Option<&'a ActiveProject>,
    default_workspace_id: WorkspaceId,
    default_project_id: ProjectId,
    /// Per-project authorization principal (#708). `None` skips the gate
    /// entirely, preserving legacy behaviour for callers that have not opted
    /// in. When set, every resolved read/write scope is run through the
    /// `authorize_project` choke point before it is returned.
    authz: Option<ProjectPrincipal>,
    /// Whether the deployment distinguishes operators; only meaningful when
    /// [`Self::authz`] is set.
    distinguishes_operators: bool,
}

/// Look up an explicit workspace/project pair without creating anything.
///
/// This free function serves surfaces like admin/web routes that do not have a
/// current-project default. [`ScopeResolver::lookup_existing`] delegates here.
pub async fn lookup_existing_scope(
    reader: &ReaderPool,
    workspace: &str,
    project: &str,
) -> Result<ResolvedScope, ScopeResolutionError> {
    let workspace_id = lookup_existing_workspace(reader, workspace).await?;
    let project_id = reader
        .find_project(workspace_id, project.to_owned())
        .await?
        .ok_or_else(|| ScopeResolutionError::ProjectNotFoundInWorkspace {
            workspace: workspace.to_owned(),
            project: project.to_owned(),
        })?;
    Ok(ResolvedScope {
        workspace_id,
        project_id,
    })
}

/// Look up an explicit workspace by name without creating anything.
///
/// This is for admin/destructive surfaces that operate at workspace granularity
/// and must fail closed on typos instead of auto-creating a scope.
pub async fn lookup_existing_workspace(
    reader: &ReaderPool,
    workspace: &str,
) -> Result<WorkspaceId, ScopeResolutionError> {
    reader
        .find_workspace(workspace.to_owned())
        .await?
        .ok_or_else(|| ScopeResolutionError::WorkspaceNotFound {
            workspace: workspace.to_owned(),
        })
}

/// Create or fetch an explicit workspace/project pair.
///
/// This is the only helper that may create a scope, and should only be used by
/// write-style paths whose public contract says they create missing projects.
pub async fn create_explicit_scope(
    writer: &WriterHandle,
    workspace: &str,
    project: &str,
) -> Result<ResolvedScope, ScopeResolutionError> {
    let workspace_id = writer.get_or_create_workspace(workspace.to_owned()).await?;
    let project_id = writer
        .get_or_create_project(workspace_id, project.to_owned(), None)
        .await?;
    Ok(ResolvedScope {
        workspace_id,
        project_id,
    })
}

/// Run a resolved scope through the per-project choke point (#708).
///
/// A read is decided on the read pool; a write on the writer actor's own
/// connection when a writer is given, so the check cannot race a concurrent
/// grant change against the write it guards.
async fn decide_scope(
    reader: &ReaderPool,
    writer: Option<&WriterHandle>,
    scope: ResolvedScope,
    principal: &ProjectPrincipal,
    distinguishes_operators: bool,
    need: ProjectAccess,
) -> Result<(), ScopeResolutionError> {
    let decision = match (need, writer) {
        (ProjectAccess::Write, Some(writer)) => {
            writer
                .authorize_project(
                    scope.workspace_id,
                    scope.project_id,
                    principal.clone(),
                    distinguishes_operators,
                    need,
                )
                .await?
        }
        _ => {
            reader
                .authorize_project(
                    scope.workspace_id,
                    scope.project_id,
                    principal.clone(),
                    distinguishes_operators,
                    need,
                )
                .await?
        }
    };
    if decision.is_ok() {
        return Ok(());
    }
    // A refusal names the project and both levels, so it says what to ask
    // for and can never be mistaken for an empty or missing project. Built on
    // the denial path only, where the extra lookups cost nothing that matters.
    let held = reader
        .resolve_project_authz(
            scope.workspace_id,
            scope.project_id,
            principal.clone(),
            distinguishes_operators,
        )
        .await?
        .grant;
    let name = reader
        .project_name_by_id(scope.workspace_id, scope.project_id)
        .await?
        .unwrap_or_else(|| "the resolved project".to_owned());
    Err(ScopeResolutionError::Forbidden(refusal_message(
        &name, held, need,
    )))
}

fn refusal_message(project: &str, held: Option<GrantLevel>, need: ProjectAccess) -> String {
    let need = match need {
        ProjectAccess::Read => "read",
        ProjectAccess::Write => "write",
    };
    match held {
        Some(held) => format!(
            "not authorized for {project}: you have {} access and this needs {need}. \
             Ask the server operator to raise your access.",
            held.as_str()
        ),
        None => format!(
            "not authorized for {project}. This is an access problem, not an empty \
             memory — ask the server operator to grant you {need} access on it."
        ),
    }
}

/// Authorize a scope resolved outside a [`ScopeResolver`] for `viewer` (#708).
///
/// For routes that name a project directly — admin, web, hook captures — and
/// so have no current-project default to resolve through. `viewer` is the
/// database user the request authenticated as; `None` (the root token, or an
/// install with no database users) skips the check, as a resolver with no
/// principal attached does.
///
/// # Errors
/// [`ScopeResolutionError::Forbidden`] when a restricted project refuses the
/// viewer; store failures otherwise.
pub async fn authorize_scope_for(
    reader: &ReaderPool,
    writer: Option<&WriterHandle>,
    scope: ResolvedScope,
    viewer: Option<UserId>,
    need: ProjectAccess,
) -> Result<ResolvedScope, ScopeResolutionError> {
    let Some(viewer) = viewer else {
        return Ok(scope);
    };
    decide_scope(
        reader,
        writer,
        scope,
        &ProjectPrincipal::user(viewer),
        true,
        need,
    )
    .await?;
    Ok(scope)
}

/// [`lookup_existing_scope`], authorized for `viewer` at `need`.
///
/// # Errors
/// As [`lookup_existing_scope`] and [`authorize_scope_for`].
pub async fn lookup_existing_scope_guarded(
    reader: &ReaderPool,
    workspace: &str,
    project: &str,
    viewer: Option<UserId>,
    need: ProjectAccess,
) -> Result<ResolvedScope, ScopeResolutionError> {
    let scope = lookup_existing_scope(reader, workspace, project).await?;
    authorize_scope_for(reader, None, scope, viewer, need).await
}

/// [`create_explicit_scope`] on behalf of `viewer`, authorized for a write.
///
/// A project this call creates records `viewer` as its creator in the same
/// transaction, and the choke point admits a creator, so they can write to and
/// read back what they just made. A project that already existed is decided
/// like any other write — "create" is never a way into somebody else's
/// restricted project, and because the writer decides which case applies,
/// there is no window between a lookup and a create for another user to win.
///
/// # Errors
/// As [`create_explicit_scope`] and [`authorize_scope_for`].
pub async fn create_explicit_scope_guarded(
    reader: &ReaderPool,
    writer: &WriterHandle,
    workspace: &str,
    project: &str,
    viewer: Option<UserId>,
) -> Result<ResolvedScope, ScopeResolutionError> {
    let workspace_id = writer.get_or_create_workspace(workspace.to_owned()).await?;
    let (project_id, _) = writer
        .get_or_create_project_as(workspace_id, project.to_owned(), None, viewer)
        .await?;
    let scope = ResolvedScope {
        workspace_id,
        project_id,
    };
    authorize_scope_for(reader, Some(writer), scope, viewer, ProjectAccess::Write).await
}

/// [`resolve_many_existing_scopes`], every scope authorized for `viewer`.
///
/// A refused scope fails the whole call rather than being dropped: silently
/// returning the permitted subset would answer a cross-project search with a
/// short list that looks like "nothing was found there", and a refusal must
/// never read as empty memory.
///
/// # Errors
/// As [`resolve_many_existing_scopes`] and [`authorize_scope_for`].
pub async fn resolve_many_existing_scopes_guarded(
    reader: &ReaderPool,
    scopes: &[ScopeName],
    max: usize,
    viewer: Option<UserId>,
    need: ProjectAccess,
) -> Result<Vec<ResolvedScope>, ScopeResolutionError> {
    let resolved = resolve_many_existing_scopes(reader, scopes, max).await?;
    for scope in &resolved {
        authorize_scope_for(reader, None, *scope, viewer, need).await?;
    }
    Ok(resolved)
}

/// Look up the reserved global preferences scope
/// ([`ai_memory_core::GLOBAL_SCOPE_PROJECT`] in the default workspace)
/// without creating it. Returns `Ok(None)` when it doesn't exist yet — the
/// scope participates in default reads by existence, so an absent scope
/// means "nothing to union in", never an error (issue #154).
///
/// Unauthorized by design, together with [`create_global_scope`]: the global
/// scope is unioned into everybody's default reads, and the unscoped-read
/// filters admit it for every viewer (#708). Per-project authorization is
/// about project scopes; the global preferences scope is common ground.
///
/// # Errors
/// Propagates store failures only; a missing workspace or project is `None`.
pub async fn lookup_global_scope(
    reader: &ReaderPool,
) -> Result<Option<ResolvedScope>, ScopeResolutionError> {
    let Some(workspace_id) = reader
        .find_workspace(ai_memory_core::DEFAULT_WORKSPACE_NAME.to_owned())
        .await?
    else {
        return Ok(None);
    };
    let Some(project_id) = reader
        .find_project(
            workspace_id,
            ai_memory_core::GLOBAL_SCOPE_PROJECT.to_owned(),
        )
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(ResolvedScope {
        workspace_id,
        project_id,
    }))
}

/// Create or fetch the reserved global preferences scope. Write-path only —
/// the counterpart of [`lookup_global_scope`] for `scope: "global"` writes.
///
/// # Errors
/// Propagates store failures.
pub async fn create_global_scope(
    writer: &WriterHandle,
) -> Result<ResolvedScope, ScopeResolutionError> {
    create_explicit_scope(
        writer,
        ai_memory_core::DEFAULT_WORKSPACE_NAME,
        ai_memory_core::GLOBAL_SCOPE_PROJECT,
    )
    .await
}

/// Resolve and de-duplicate explicit multi-scope names without creating
/// anything. Surfaces that do not have a current-project default (admin/web)
/// can call this directly; [`ScopeResolver::resolve_many_existing`] delegates
/// here.
pub async fn resolve_many_existing_scopes(
    reader: &ReaderPool,
    scopes: &[ScopeName],
    max: usize,
) -> Result<Vec<ResolvedScope>, ScopeResolutionError> {
    if scopes.len() > max {
        return Err(ScopeResolutionError::TooManyScopes {
            max,
            actual: scopes.len(),
        });
    }
    let mut seen = HashSet::new();
    let mut resolved = Vec::new();
    for scope in scopes {
        let workspace =
            trimmed_opt(Some(&scope.workspace)).ok_or(ScopeResolutionError::ScopeWorkspaceEmpty)?;
        let project =
            trimmed_opt(Some(&scope.project)).ok_or(ScopeResolutionError::ScopeProjectEmpty)?;
        let ids = lookup_existing_scope(reader, workspace, project).await?;
        if seen.insert(ids) {
            resolved.push(ids);
        }
    }
    Ok(resolved)
}

impl<'a> ScopeResolver<'a> {
    /// Build a resolver for read-only policies.
    #[must_use]
    pub fn new(
        reader: &'a ReaderPool,
        default_workspace_id: WorkspaceId,
        default_project_id: ProjectId,
    ) -> Self {
        Self {
            reader,
            writer: None,
            active_project: None,
            default_workspace_id,
            default_project_id,
            authz: None,
            distinguishes_operators: false,
        }
    }

    /// Attach the writer handle needed by create-on-write resolution.
    #[must_use]
    pub fn with_writer(mut self, writer: &'a WriterHandle) -> Self {
        self.writer = Some(writer);
        self
    }

    /// Opt into the per-project authorization choke point (#708).
    ///
    /// Once attached, every scope this resolver returns is run through
    /// `authorize_project`: reads through the read pool, writes through the
    /// writer actor (defense in depth) when a writer is attached. In slice 2
    /// every project is `open`, so this is a behaviour-preserving pass-through.
    #[must_use]
    pub fn with_project_authz(
        mut self,
        principal: ProjectPrincipal,
        distinguishes_operators: bool,
    ) -> Self {
        self.authz = Some(principal);
        self.distinguishes_operators = distinguishes_operators;
        self
    }

    /// Run the resolved scope through the authorization choke point (#708).
    ///
    /// A no-op when no principal is attached. A read is decided on the read
    /// pool; a write is decided on the writer actor's own connection when a
    /// writer is attached, so the check cannot race a concurrent grant change
    /// against the write it guards.
    async fn authorize_scope(
        &self,
        scope: ResolvedScope,
        need: ProjectAccess,
    ) -> Result<(), ScopeResolutionError> {
        let Some(principal) = self.authz.as_ref() else {
            return Ok(());
        };
        decide_scope(
            self.reader,
            self.writer,
            scope,
            principal,
            self.distinguishes_operators,
            need,
        )
        .await
    }

    /// Attach the active-project map used for current-project defaults.
    #[must_use]
    pub fn with_active_project(mut self, active_project: &'a ActiveProject) -> Self {
        self.active_project = Some(active_project);
        self
    }

    /// Look up an explicit workspace/project pair without creating anything.
    /// Used by read, maintenance, and destructive paths.
    pub async fn lookup_existing(
        &self,
        workspace: &str,
        project: &str,
    ) -> Result<ResolvedScope, ScopeResolutionError> {
        lookup_existing_scope(self.reader, workspace, project).await
    }

    /// Resolve MCP-style read arguments: explicit pair if both names are
    /// provided, reject partial pair, otherwise use project-only lookup or the
    /// current-project/default fallback chain.
    pub async fn resolve_read_args(
        &self,
        explicit_workspace: Option<&str>,
        explicit_project: Option<&str>,
        actor: &ActorKey,
    ) -> Result<ResolvedScope, ScopeResolutionError> {
        self.resolve_existing_args(
            explicit_workspace,
            explicit_project,
            actor,
            ProjectAccess::Read,
        )
        .await
    }

    /// [`Self::resolve_read_args`], plus where the scope came from.
    pub async fn resolve_read_args_traced(
        &self,
        explicit_workspace: Option<&str>,
        explicit_project: Option<&str>,
        actor: &ActorKey,
    ) -> Result<(ResolvedScope, ScopeSource), ScopeResolutionError> {
        self.resolve_existing_args_traced(
            explicit_workspace,
            explicit_project,
            actor,
            ProjectAccess::Read,
        )
        .await
    }

    /// Resolve read-shaped arguments naming an EXISTING scope, authorized at
    /// `need` (#708).
    ///
    /// The argument shape is not the access level. Tools that delete a page,
    /// record feedback, sweep, lint, accept a handoff or pop a message take the
    /// same arguments as a read and all mutate; resolving them as a read would
    /// let a `read` grant do each of those. The shape says which scope; `need`
    /// says what the caller may do to it. Never creates — that is
    /// [`Self::resolve_write_args`].
    pub async fn resolve_existing_args(
        &self,
        explicit_workspace: Option<&str>,
        explicit_project: Option<&str>,
        actor: &ActorKey,
        need: ProjectAccess,
    ) -> Result<ResolvedScope, ScopeResolutionError> {
        self.resolve_existing_args_traced(explicit_workspace, explicit_project, actor, need)
            .await
            .map(|(scope, _)| scope)
    }

    /// [`Self::resolve_existing_args`], plus where the scope came from.
    pub async fn resolve_existing_args_traced(
        &self,
        explicit_workspace: Option<&str>,
        explicit_project: Option<&str>,
        actor: &ActorKey,
        need: ProjectAccess,
    ) -> Result<(ResolvedScope, ScopeSource), ScopeResolutionError> {
        match (
            trimmed_opt(explicit_workspace),
            trimmed_opt(explicit_project),
        ) {
            (Some(workspace), Some(project)) => {
                let scope = self.lookup_existing(workspace, project).await?;
                self.authorize_scope(scope, need).await?;
                Ok((scope, ScopeSource::Explicit))
            }
            (Some(_), None) => Err(ScopeResolutionError::WorkspaceProjectPairRequired),
            (None, project) => {
                self.resolve_current_or_project_traced(project, actor, need)
                    .await
            }
        }
    }

    /// Resolve a project-only read, or the current/default project when no
    /// project was supplied.
    pub async fn resolve_current_or_project(
        &self,
        explicit_project: Option<&str>,
        actor: &ActorKey,
    ) -> Result<ResolvedScope, ScopeResolutionError> {
        self.resolve_current_or_project_traced(explicit_project, actor, ProjectAccess::Read)
            .await
            .map(|(scope, _)| scope)
    }

    async fn resolve_current_or_project_traced(
        &self,
        explicit_project: Option<&str>,
        actor: &ActorKey,
        need: ProjectAccess,
    ) -> Result<(ResolvedScope, ScopeSource), ScopeResolutionError> {
        // Read path, so `read_pointer`: it adds the startup seed for a caller
        // the pointer knows nothing about, which is every caller in the window
        // between a restart and the first hook event (#678). `resolve_write_args`
        // below deliberately stays on `get_for` / `lookup_for` — a write must
        // not be attributed to a project reconstructed from history the caller
        // never named.
        let pointer = self
            .active_project
            .map_or(ReadPointer::Unset, |a| a.read_pointer(actor));
        let source = match pointer {
            ReadPointer::Session(..) => ScopeSource::Session,
            ReadPointer::SharedSlot(..) => ScopeSource::SharedSlot,
            ReadPointer::StartupSeed(..) => ScopeSource::StartupSeed,
            ReadPointer::Mismatch => ScopeSource::DefaultAfterMismatch,
            ReadPointer::Unset => ScopeSource::Default,
        };
        let active = pointer.ids();
        if let Some(project) = trimmed_opt(explicit_project) {
            if let Some((active_ws, _)) = active
                && let Some(project_id) = self
                    .reader
                    .find_project(active_ws, project.to_owned())
                    .await?
            {
                let scope = ResolvedScope {
                    workspace_id: active_ws,
                    project_id,
                };
                self.authorize_scope(scope, need).await?;
                return Ok((scope, ScopeSource::Explicit));
            }
            if active.map(|(ws, _)| ws) != Some(self.default_workspace_id)
                && let Some(project_id) = self
                    .reader
                    .find_project(self.default_workspace_id, project.to_owned())
                    .await?
            {
                let scope = ResolvedScope {
                    workspace_id: self.default_workspace_id,
                    project_id,
                };
                self.authorize_scope(scope, need).await?;
                return Ok((scope, ScopeSource::Explicit));
            }
            return Err(ScopeResolutionError::ProjectNotFoundInActiveOrDefault {
                project: project.to_owned(),
            });
        }
        let (workspace_id, project_id) =
            active.unwrap_or((self.default_workspace_id, self.default_project_id));
        let scope = ResolvedScope {
            workspace_id,
            project_id,
        };
        self.authorize_scope(scope, need).await?;
        Ok((scope, source))
    }

    /// Resolve a write target. Explicit names may create the workspace/project;
    /// absence means current-project/default. Partial explicit scopes fail.
    pub async fn resolve_write_args(
        &self,
        explicit_workspace: Option<&str>,
        explicit_project: Option<&str>,
        actor: &ActorKey,
    ) -> Result<ResolvedScope, ScopeResolutionError> {
        let Some(project) = trimmed_opt(explicit_project) else {
            if trimmed_opt(explicit_workspace).is_some() {
                return Err(ScopeResolutionError::WorkspaceProjectPairRequired);
            }
            // #564: a caller carrying a coordinate that resolves to nothing is not
            // asking for the server default — it is a mismatch, and the page it
            // writes would be real, attributed, searchable, and in a project nobody
            // looks in. Refuse and name the two fixes. Callers with no coordinate at
            // all (anonymous/legacy) keep resolving through the default as before.
            let (workspace_id, project_id) = match self.active_project.map(|a| a.lookup_for(actor))
            {
                Some(ActiveProjectLookup::Resolved(workspace_id, project_id)) => {
                    (workspace_id, project_id)
                }
                Some(ActiveProjectLookup::Mismatch) => {
                    return Err(ScopeResolutionError::AmbiguousUnscopedWrite);
                }
                Some(ActiveProjectLookup::Unset) | None => {
                    (self.default_workspace_id, self.default_project_id)
                }
            };
            let scope = ResolvedScope {
                workspace_id,
                project_id,
            };
            self.authorize_scope(scope, ProjectAccess::Write).await?;
            return Ok(scope);
        };
        let Some(writer) = self.writer else {
            return Err(ScopeResolutionError::WriterRequired);
        };
        let active = self.active_project.and_then(|a| a.get_for(actor));
        let workspace_id = match trimmed_opt(explicit_workspace) {
            Some(workspace) => writer.get_or_create_workspace(workspace.to_owned()).await?,
            None => active
                .map(|(workspace_id, _)| workspace_id)
                .unwrap_or(self.default_workspace_id),
        };
        // A project this call creates records the caller as its creator, whom
        // the choke point below then admits; one that already existed is
        // decided like any other write, so "create" is never a way in.
        let creator = self.authz.as_ref().and_then(|principal| principal.user_id);
        let (project_id, _) = writer
            .get_or_create_project_as(workspace_id, project.to_owned(), None, creator)
            .await?;
        let scope = ResolvedScope {
            workspace_id,
            project_id,
        };
        self.authorize_scope(scope, ProjectAccess::Write).await?;
        Ok(scope)
    }

    /// Resolve and de-duplicate an explicit multi-scope list, every scope
    /// authorized for a read.
    ///
    /// A refused scope fails the whole call rather than being dropped — see
    /// [`resolve_many_existing_scopes_guarded`].
    pub async fn resolve_many_existing(
        &self,
        scopes: &[ScopeName],
        max: usize,
    ) -> Result<Vec<ResolvedScope>, ScopeResolutionError> {
        let resolved = resolve_many_existing_scopes(self.reader, scopes, max).await?;
        for scope in &resolved {
            self.authorize_scope(*scope, ProjectAccess::Read).await?;
        }
        Ok(resolved)
    }
}

fn trimmed_opt(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use ai_memory_core::NewUser;

    use crate::{AccessMode, GrantLevel};

    async fn user_named(store: &Store, username: &str, byte: u8) -> UserId {
        store
            .writer
            .create_user(
                NewUser {
                    username: username.to_owned(),
                    name: None,
                    email: None,
                },
                [byte; crate::TOKEN_HASH_LEN],
            )
            .await
            .unwrap()
    }

    /// A restricted project plus two users, neither holding anything on it.
    async fn guard_fixture(store: &Store) -> (WorkspaceId, ProjectId, UserId, UserId) {
        // Grants only decide anything in a restricted project; every project
        // these tests create starts restricted.
        store
            .writer
            .set_new_project_mode(AccessMode::Restricted)
            .await
            .unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project = store
            .writer
            .get_or_create_project(ws, "client-work", None)
            .await
            .unwrap();
        let alice = user_named(store, "alice", 1).await;
        let bob = user_named(store, "bob", 2).await;
        (ws, project, alice, bob)
    }

    async fn grant(store: &Store, user: UserId, project: ProjectId, level: GrantLevel) {
        store
            .writer
            .grant_memory(user, project, level, None)
            .await
            .unwrap();
    }

    fn created_by(store: &Store, project: ProjectId) -> Option<UserId> {
        let conn = rusqlite::Connection::open(store.db_path()).unwrap();
        conn.query_row(
            "SELECT created_by FROM projects WHERE id = ?1",
            rusqlite::params![project.as_bytes()],
            |row| row.get::<_, Option<Vec<u8>>>(0),
        )
        .unwrap()
        .map(|raw| UserId::from_slice(&raw).unwrap())
    }

    fn as_user<'a>(
        store: &'a Store,
        ws: WorkspaceId,
        project: ProjectId,
        user: UserId,
    ) -> ScopeResolver<'a> {
        ScopeResolver::new(&store.reader, ws, project)
            .with_writer(&store.writer)
            .with_project_authz(ProjectPrincipal::user(user), true)
    }

    /// The argument shape and the level are independent, and must stay so.
    ///
    /// Mutating tools take the same arguments as reads. Resolving those as a
    /// read is what would let a `read` grant delete pages, so the level is the
    /// caller's to state — on the explicit-pair branch and on the
    /// current-project fallback alike.
    #[tokio::test]
    async fn the_argument_shape_does_not_decide_the_level() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, project, alice, _) = guard_fixture(&store).await;
        grant(&store, alice, project, GrantLevel::Read).await;
        let resolver = as_user(&store, ws, project, alice);
        let actor = ActorKey::default();

        for (label, workspace, name) in [
            ("explicit pair", Some("default"), Some("client-work")),
            ("current-project fallback", None, None),
        ] {
            resolver
                .resolve_existing_args(workspace, name, &actor, ProjectAccess::Read)
                .await
                .unwrap_or_else(|e| panic!("{label}: a reader may read: {e}"));
            let err = resolver
                .resolve_existing_args(workspace, name, &actor, ProjectAccess::Write)
                .await
                .unwrap_err();
            assert!(
                err.is_forbidden(),
                "{label}: a reader must not reach a write: {err:?}"
            );
        }
    }

    /// Root and an install with no database users both arrive as no viewer,
    /// and neither may change behaviour.
    #[tokio::test]
    async fn no_viewer_resolves_exactly_as_before() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, project, _, _) = guard_fixture(&store).await;

        let scope = lookup_existing_scope_guarded(
            &store.reader,
            "default",
            "client-work",
            None,
            ProjectAccess::Write,
        )
        .await
        .unwrap();
        assert_eq!(
            scope,
            ResolvedScope {
                workspace_id: ws,
                project_id: project
            }
        );
    }

    #[tokio::test]
    async fn a_user_reaches_only_what_they_were_granted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (_, project, alice, bob) = guard_fixture(&store).await;
        grant(&store, alice, project, GrantLevel::Read).await;
        let lookup = |user, need| {
            lookup_existing_scope_guarded(&store.reader, "default", "client-work", Some(user), need)
        };

        lookup(alice, ProjectAccess::Read).await.unwrap();
        assert!(
            lookup(alice, ProjectAccess::Write)
                .await
                .unwrap_err()
                .is_forbidden()
        );
        // Bob holds nothing. The refusal must say so — not resolve to an
        // empty project, which would read as "there is nothing here".
        let err = lookup(bob, ProjectAccess::Read).await.unwrap_err();
        assert!(err.is_forbidden(), "{err:?}");
        assert!(!err.is_not_found());
    }

    #[tokio::test]
    async fn creating_authorizes_against_a_project_that_already_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, project, alice, bob) = guard_fixture(&store).await;
        grant(&store, alice, project, GrantLevel::Read).await;
        let create = |name: &'static str, user| {
            create_explicit_scope_guarded(&store.reader, &store.writer, "default", name, Some(user))
        };

        // "Create" is not a way around the choke point: the project is already
        // there, so Bob's write is refused exactly as a read would be, and
        // Alice's read grant does not cover a write.
        assert!(create("client-work", bob).await.unwrap_err().is_forbidden());
        assert!(
            create("client-work", alice)
                .await
                .unwrap_err()
                .is_forbidden()
        );
        assert_eq!(created_by(&store, project), None);

        // A project that does not exist yet is created with Bob as its
        // creator, who is admitted without any grant.
        let fresh = create("brand-new", bob).await.unwrap();
        assert_eq!(fresh.workspace_id, ws);
        assert_eq!(created_by(&store, fresh.project_id), Some(bob));
        assert!(
            store
                .reader
                .grants_for(bob, fresh.project_id)
                .await
                .unwrap()
                .is_empty(),
            "the creator needs no grant"
        );
        lookup_existing_scope_guarded(
            &store.reader,
            "default",
            "brand-new",
            Some(bob),
            ProjectAccess::Write,
        )
        .await
        .expect("the creator reaches what they created");

        // A second "create" of the same name by someone else is a write to an
        // existing project: refused, and it does not make them its creator.
        assert!(create("brand-new", alice).await.unwrap_err().is_forbidden());
        assert_eq!(created_by(&store, fresh.project_id), Some(bob));
    }

    /// With no viewer — root, or an install with no database users — a created
    /// project records no creator: there is no user to attribute it to.
    #[tokio::test]
    async fn creating_without_a_viewer_records_no_creator() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        guard_fixture(&store).await;
        let fresh = create_explicit_scope_guarded(
            &store.reader,
            &store.writer,
            "default",
            "unattributed",
            None,
        )
        .await
        .unwrap();
        assert_eq!(created_by(&store, fresh.project_id), None);
    }

    /// The MCP write path creates through the resolver, not the free function,
    /// and must behave the same.
    #[tokio::test]
    async fn write_args_record_the_creator_and_check_everyone_else() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, project, alice, bob) = guard_fixture(&store).await;
        let actor = ActorKey::default();

        let as_bob = as_user(&store, ws, project, bob);
        let created = as_bob
            .resolve_write_args(Some("default"), Some("bobs-repo"), &actor)
            .await
            .unwrap();
        assert_eq!(created_by(&store, created.project_id), Some(bob));
        as_bob
            .resolve_write_args(Some("default"), Some("bobs-repo"), &actor)
            .await
            .expect("the creator writes to it again");

        let err = as_user(&store, ws, project, alice)
            .resolve_write_args(Some("default"), Some("bobs-repo"), &actor)
            .await
            .unwrap_err();
        assert!(err.is_forbidden(), "{err:?}");
    }

    #[tokio::test]
    async fn a_refused_scope_fails_the_search_instead_of_shortening_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, granted, alice, _) = guard_fixture(&store).await;
        let refused = store
            .writer
            .get_or_create_project(ws, "other-team", None)
            .await
            .unwrap();
        grant(&store, alice, granted, GrantLevel::Read).await;
        let names = [
            ScopeName::new("default", "client-work"),
            ScopeName::new("default", "client-work"),
            ScopeName::new("default", "other-team"),
        ];

        let err = resolve_many_existing_scopes_guarded(
            &store.reader,
            &names,
            25,
            Some(alice),
            ProjectAccess::Read,
        )
        .await
        .unwrap_err();
        assert!(err.is_forbidden(), "{err:?}");
        let err = as_user(&store, ws, granted, alice)
            .resolve_many_existing(&names, 25)
            .await
            .unwrap_err();
        assert!(err.is_forbidden(), "the resolver agrees: {err:?}");

        // Every scope granted: the call succeeds, still de-duplicated.
        grant(&store, alice, refused, GrantLevel::Read).await;
        let resolved = as_user(&store, ws, granted, alice)
            .resolve_many_existing(&names, 25)
            .await
            .unwrap();
        assert_eq!(resolved.len(), 2);
    }

    #[tokio::test]
    async fn a_revoked_grant_denies_like_one_never_granted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (_, project, alice, bob) = guard_fixture(&store).await;
        store
            .writer
            .grant_memory(alice, project, GrantLevel::Write, Some(bob))
            .await
            .unwrap();
        assert!(
            store
                .writer
                .revoke_memory(alice, project, Some(bob))
                .await
                .unwrap()
        );
        let err = lookup_existing_scope_guarded(
            &store.reader,
            "default",
            "client-work",
            Some(alice),
            ProjectAccess::Read,
        )
        .await
        .unwrap_err();
        assert!(err.is_forbidden(), "{err:?}");
    }

    #[tokio::test]
    async fn read_args_reject_partial_scope() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let resolver = ScopeResolver::new(&store.reader, ws, project);
        let err = resolver
            .resolve_read_args(Some("default"), None, &ActorKey::default())
            .await
            .unwrap_err();
        assert_eq!(err, ScopeResolutionError::WorkspaceProjectPairRequired);
        assert_eq!(err.to_string(), WORKSPACE_PROJECT_PAIR_REQUIRED);
    }

    #[tokio::test]
    async fn project_only_read_prefers_active_workspace_then_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let default_ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let default_scratch = store
            .writer
            .get_or_create_project(default_ws, "scratch", None)
            .await
            .unwrap();
        let active_ws = store.writer.get_or_create_workspace("team").await.unwrap();
        let active_scratch = store
            .writer
            .get_or_create_project(active_ws, "scratch", None)
            .await
            .unwrap();
        let active_project = ActiveProject::new();
        let actor = ActorKey {
            user: Some("alice".into()),
            session_id: Some("s1".into()),
        };
        active_project.set_for(&actor, active_ws, active_scratch, false);

        let resolver = ScopeResolver::new(&store.reader, default_ws, default_scratch)
            .with_active_project(&active_project);
        let scope = resolver
            .resolve_read_args(None, Some("scratch"), &actor)
            .await
            .unwrap();
        assert_eq!(scope.as_tuple(), (active_ws, active_scratch));

        let err = resolver
            .resolve_read_args(None, Some("missing"), &actor)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ScopeResolutionError::ProjectNotFoundInActiveOrDefault {
                project: "missing".into()
            }
        );
    }

    #[tokio::test]
    async fn write_args_create_project_in_active_workspace() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let default_ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let default_project = store
            .writer
            .get_or_create_project(default_ws, "scratch", None)
            .await
            .unwrap();
        let active_ws = store.writer.get_or_create_workspace("team").await.unwrap();
        let active_project_id = store
            .writer
            .get_or_create_project(active_ws, "current", None)
            .await
            .unwrap();
        let active_project = ActiveProject::new();
        let actor = ActorKey {
            user: None,
            session_id: Some("s1".into()),
        };
        active_project.set_for(&actor, active_ws, active_project_id, false);
        let resolver = ScopeResolver::new(&store.reader, default_ws, default_project)
            .with_writer(&store.writer)
            .with_active_project(&active_project);

        let created = resolver
            .resolve_write_args(None, Some("new-project"), &actor)
            .await
            .unwrap();
        assert_eq!(created.workspace_id, active_ws);
        assert!(
            store
                .reader
                .find_project(default_ws, "new-project".into())
                .await
                .unwrap()
                .is_none(),
            "project-only writes must not recreate the baked default workspace"
        );
        assert_eq!(
            store
                .reader
                .find_project(active_ws, "new-project".into())
                .await
                .unwrap(),
            Some(created.project_id)
        );
    }

    #[tokio::test]
    async fn multi_scope_resolution_deduplicates_and_validates() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let resolver = ScopeResolver::new(&store.reader, ws, project);
        let scopes = vec![
            ScopeName::new("default", "scratch"),
            ScopeName::new(" default ", " scratch "),
        ];
        let resolved = resolver.resolve_many_existing(&scopes, 25).await.unwrap();
        assert_eq!(
            resolved,
            vec![ResolvedScope {
                workspace_id: ws,
                project_id: project
            }]
        );

        let err = resolver
            .resolve_many_existing(&[ScopeName::new("", "scratch")], 25)
            .await
            .unwrap_err();
        assert_eq!(err, ScopeResolutionError::ScopeWorkspaceEmpty);

        let err = resolver
            .resolve_many_existing(
                &[
                    ScopeName::new("default", "scratch"),
                    ScopeName::new("default", "scratch"),
                ],
                1,
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ScopeResolutionError::TooManyScopes { max: 1, actual: 2 }
        );
    }

    #[tokio::test]
    async fn global_scope_lookup_is_none_until_created_then_stable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();

        // Reads never materialise the reserved scope.
        assert_eq!(lookup_global_scope(&store.reader).await.unwrap(), None);
        assert_eq!(
            lookup_global_scope(&store.reader).await.unwrap(),
            None,
            "lookup must stay a pure read"
        );

        // The write path creates it once; lookup then resolves the same ids.
        let created = create_global_scope(&store.writer).await.unwrap();
        let looked_up = lookup_global_scope(&store.reader).await.unwrap();
        assert_eq!(looked_up, Some(created));

        // Idempotent create.
        let again = create_global_scope(&store.writer).await.unwrap();
        assert_eq!(again, created);
    }

    /// Expected outcome for one table-driven read-resolution row.
    #[derive(Debug)]
    enum Expected {
        Resolved(WorkspaceId, ProjectId),
        Failed(ScopeResolutionError),
    }

    struct ReadCase {
        name: &'static str,
        workspace: Option<&'static str>,
        project: Option<&'static str>,
        active: Option<(WorkspaceId, ProjectId)>,
        expected: Expected,
    }

    /// AGENTS.md mandates a table-driven suite over the scope-resolution
    /// policies: partial scope, missing explicit scope, active-project
    /// precedence, and cross-workspace isolation. One fixture with two
    /// workspaces carrying a same-named project feeds every row.
    #[tokio::test]
    async fn read_resolution_table_driven() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();

        let default_ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let default_scratch = store
            .writer
            .get_or_create_project(default_ws, "scratch", None)
            .await
            .unwrap();
        let alpha_ws = store.writer.get_or_create_workspace("alpha").await.unwrap();
        let alpha_app = store
            .writer
            .get_or_create_project(alpha_ws, "app", None)
            .await
            .unwrap();
        let alpha_shared = store
            .writer
            .get_or_create_project(alpha_ws, "shared", None)
            .await
            .unwrap();
        let beta_ws = store.writer.get_or_create_workspace("beta").await.unwrap();
        let beta_shared = store
            .writer
            .get_or_create_project(beta_ws, "shared", None)
            .await
            .unwrap();
        let beta_other = store
            .writer
            .get_or_create_project(beta_ws, "other", None)
            .await
            .unwrap();
        assert_ne!(
            alpha_shared, beta_shared,
            "same-named projects must get distinct ids per workspace"
        );

        let cases = vec![
            // --- Partial scope fails closed ---
            ReadCase {
                name: "workspace without project is rejected",
                workspace: Some("alpha"),
                project: None,
                active: Some((alpha_ws, alpha_app)),
                expected: Expected::Failed(ScopeResolutionError::WorkspaceProjectPairRequired),
            },
            ReadCase {
                name: "project-only read does not scan every workspace",
                workspace: None,
                project: Some("app"), // exists only in alpha; no active pointer
                active: None,
                expected: Expected::Failed(
                    ScopeResolutionError::ProjectNotFoundInActiveOrDefault {
                        project: "app".into(),
                    },
                ),
            },
            ReadCase {
                name: "project-only read of a missing project fails closed",
                workspace: None,
                project: Some("ghost"),
                active: Some((alpha_ws, alpha_app)),
                expected: Expected::Failed(
                    ScopeResolutionError::ProjectNotFoundInActiveOrDefault {
                        project: "ghost".into(),
                    },
                ),
            },
            // --- Missing explicit scope errors without creating ---
            ReadCase {
                name: "missing workspace errors",
                workspace: Some("ghost"),
                project: Some("app"),
                active: None,
                expected: Expected::Failed(ScopeResolutionError::WorkspaceNotFound {
                    workspace: "ghost".into(),
                }),
            },
            ReadCase {
                name: "missing project in existing workspace errors",
                workspace: Some("alpha"),
                project: Some("ghost"),
                active: Some((alpha_ws, alpha_app)),
                expected: Expected::Failed(ScopeResolutionError::ProjectNotFoundInWorkspace {
                    workspace: "alpha".into(),
                    project: "ghost".into(),
                }),
            },
            // --- Active-project precedence ---
            ReadCase {
                name: "explicit pair beats active project",
                workspace: Some("beta"),
                project: Some("other"),
                active: Some((alpha_ws, alpha_app)),
                expected: Expected::Resolved(beta_ws, beta_other),
            },
            ReadCase {
                name: "no args resolves the active project",
                workspace: None,
                project: None,
                active: Some((alpha_ws, alpha_app)),
                expected: Expected::Resolved(alpha_ws, alpha_app),
            },
            ReadCase {
                name: "no args and no active resolves the default",
                workspace: None,
                project: None,
                active: None,
                expected: Expected::Resolved(default_ws, default_scratch),
            },
            ReadCase {
                name: "whitespace-only args resolve the default",
                workspace: Some("  "),
                project: Some(" "),
                active: None,
                expected: Expected::Resolved(default_ws, default_scratch),
            },
            ReadCase {
                name: "project-only prefers the active workspace",
                workspace: None,
                project: Some("shared"),
                active: Some((alpha_ws, alpha_app)),
                expected: Expected::Resolved(alpha_ws, alpha_shared),
            },
            ReadCase {
                name: "project-only falls back to the default workspace",
                workspace: None,
                project: Some("scratch"), // exists only in default
                active: Some((alpha_ws, alpha_app)),
                expected: Expected::Resolved(default_ws, default_scratch),
            },
            // --- Cross-workspace isolation ---
            ReadCase {
                name: "shared resolves in alpha when alpha is named",
                workspace: Some("alpha"),
                project: Some("shared"),
                active: None,
                expected: Expected::Resolved(alpha_ws, alpha_shared),
            },
            ReadCase {
                name: "shared resolves in beta when beta is named",
                workspace: Some("beta"),
                project: Some("shared"),
                active: None,
                expected: Expected::Resolved(beta_ws, beta_shared),
            },
            ReadCase {
                name: "project-only with beta active stays in beta",
                workspace: None,
                project: Some("shared"),
                active: Some((beta_ws, beta_other)),
                expected: Expected::Resolved(beta_ws, beta_shared),
            },
        ];

        let actor = ActorKey {
            user: Some("alice".into()),
            session_id: Some("s1".into()),
        };
        for case in &cases {
            let active_project = ActiveProject::new();
            if let Some((ws, proj)) = case.active {
                active_project.set_for(&actor, ws, proj, false);
            }
            let resolver = ScopeResolver::new(&store.reader, default_ws, default_scratch)
                .with_active_project(&active_project);
            let result = resolver
                .resolve_read_args(case.workspace, case.project, &actor)
                .await;
            match (&result, &case.expected) {
                (Ok(scope), Expected::Resolved(ws, proj)) => {
                    assert_eq!(
                        scope.as_tuple(),
                        (*ws, *proj),
                        "case '{}' resolved the wrong scope",
                        case.name
                    );
                }
                (Err(err), Expected::Failed(expected)) => {
                    assert_eq!(err, expected, "case '{}' failed the wrong way", case.name);
                }
                (result, expected) => panic!(
                    "case '{}': expected {expected:?}, got {result:?}",
                    case.name
                ),
            }
        }

        // The failing read rows must not have auto-created anything.
        assert!(
            store
                .reader
                .find_workspace("ghost".into())
                .await
                .unwrap()
                .is_none(),
            "read resolution must never create workspaces"
        );
        assert!(
            store
                .reader
                .find_project(alpha_ws, "ghost".into())
                .await
                .unwrap()
                .is_none(),
            "read resolution must never create projects"
        );

        // The write-style helper is the only path that may create, and once it
        // does, the no-create lookup resolves the same ids.
        let created = create_explicit_scope(&store.writer, "ghost", "app")
            .await
            .unwrap();
        assert_eq!(
            lookup_existing_scope(&store.reader, "ghost", "app")
                .await
                .unwrap(),
            created
        );

        // Multi-scope resolution keeps same-named projects in their own
        // workspaces and fails closed when one entry is missing.
        let resolver = ScopeResolver::new(&store.reader, default_ws, default_scratch);
        let both = resolver
            .resolve_many_existing(
                &[
                    ScopeName::new("alpha", "shared"),
                    ScopeName::new("beta", "shared"),
                ],
                25,
            )
            .await
            .unwrap();
        assert_eq!(
            both,
            vec![
                ResolvedScope {
                    workspace_id: alpha_ws,
                    project_id: alpha_shared
                },
                ResolvedScope {
                    workspace_id: beta_ws,
                    project_id: beta_shared
                },
            ]
        );
        let err = resolver
            .resolve_many_existing(
                &[
                    ScopeName::new("alpha", "shared"),
                    ScopeName::new("beta", "ghost"),
                ],
                25,
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ScopeResolutionError::ProjectNotFoundInWorkspace {
                workspace: "beta".into(),
                project: "ghost".into()
            }
        );
    }

    /// Build a store with a `default/scratch` fallback plus an `ActiveProject`.
    async fn scoped_fixture(
        tmp: &tempfile::TempDir,
    ) -> (Store, WorkspaceId, ProjectId, WorkspaceId, ProjectId) {
        let store = Store::open(tmp.path()).unwrap();
        let default_ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let default_proj = store
            .writer
            .get_or_create_project(default_ws, "scratch", None)
            .await
            .unwrap();
        let team_ws = store.writer.get_or_create_workspace("team").await.unwrap();
        let team_proj = store
            .writer
            .get_or_create_project(team_ws, "real-work", None)
            .await
            .unwrap();
        (store, default_ws, default_proj, team_ws, team_proj)
    }

    #[tokio::test]
    async fn unscoped_write_with_unresolvable_coordinate_errors() {
        // #564: a caller that HAS a coordinate but whose pointer misses used to be
        // written into the server default — a real, attributed, searchable page that
        // nobody goes looking for. Refuse instead.
        let tmp = tempfile::TempDir::new().unwrap();
        let (store, default_ws, default_proj, team_ws, team_proj) = scoped_fixture(&tmp).await;

        let active_project = ActiveProject::new();
        let publisher = ActorKey {
            user: Some("alice".into()),
            session_id: Some("s1".into()),
        };
        active_project.set_for(&publisher, team_ws, team_proj, false);

        // A different operator entirely: carries a coordinate, matches nothing.
        let stranger = ActorKey {
            user: Some("bob".into()),
            session_id: Some("s9".into()),
        };

        let resolver = ScopeResolver::new(&store.reader, default_ws, default_proj)
            .with_writer(&store.writer)
            .with_active_project(&active_project);

        let err = resolver
            .resolve_write_args(None, None, &stranger)
            .await
            .unwrap_err();
        assert_eq!(err, ScopeResolutionError::AmbiguousUnscopedWrite);
        assert!(err.is_bad_request());
    }

    #[tokio::test]
    async fn unscoped_write_without_any_coordinate_uses_the_shared_slot() {
        // Anonymous/legacy callers resolve through the shared slot by design and must
        // keep working — the error is only for a caller that HAS a coordinate. Note
        // this resolves to the last published pointer, not the server default:
        // `set_for` writes the shared slot alongside the per-actor entry.
        let tmp = tempfile::TempDir::new().unwrap();
        let (store, default_ws, default_proj, team_ws, team_proj) = scoped_fixture(&tmp).await;

        let active_project = ActiveProject::new();
        let publisher = ActorKey {
            user: Some("alice".into()),
            session_id: Some("s1".into()),
        };
        active_project.set_for(&publisher, team_ws, team_proj, false);

        let resolver = ScopeResolver::new(&store.reader, default_ws, default_proj)
            .with_writer(&store.writer)
            .with_active_project(&active_project);

        let scope = resolver
            .resolve_write_args(None, None, &ActorKey::default())
            .await
            .unwrap();
        assert_eq!(scope.as_tuple(), (team_ws, team_proj));
    }

    #[tokio::test]
    async fn unscoped_write_on_an_install_with_no_pointer_uses_the_default() {
        // An install with no lifecycle hooks feeding the pointer has never keyed
        // anything and has an empty shared slot. There is no better information
        // anywhere, so the configured default stays the answer — including for a
        // caller that does carry a coordinate.
        let tmp = tempfile::TempDir::new().unwrap();
        let (store, default_ws, default_proj, _team_ws, _team_proj) = scoped_fixture(&tmp).await;

        let active_project = ActiveProject::new();
        let actor = ActorKey {
            user: Some("alice".into()),
            session_id: Some("s1".into()),
        };

        let resolver = ScopeResolver::new(&store.reader, default_ws, default_proj)
            .with_writer(&store.writer)
            .with_active_project(&active_project);

        let scope = resolver
            .resolve_write_args(None, None, &actor)
            .await
            .unwrap();
        assert_eq!(scope.as_tuple(), (default_ws, default_proj));
    }

    #[tokio::test]
    async fn unscoped_write_with_resolving_pointer_uses_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (store, default_ws, default_proj, team_ws, team_proj) = scoped_fixture(&tmp).await;

        let active_project = ActiveProject::new();
        let actor = ActorKey {
            user: Some("alice".into()),
            session_id: Some("s1".into()),
        };
        active_project.set_for(&actor, team_ws, team_proj, false);

        let resolver = ScopeResolver::new(&store.reader, default_ws, default_proj)
            .with_writer(&store.writer)
            .with_active_project(&active_project);

        let scope = resolver
            .resolve_write_args(None, None, &actor)
            .await
            .unwrap();
        assert_eq!(scope.as_tuple(), (team_ws, team_proj));
    }

    #[tokio::test]
    async fn unscoped_read_with_unresolvable_coordinate_still_falls_back() {
        // Reads keep the fallback: a read answering from the default project is a
        // wrong answer the caller can see; a write is a misfile they cannot.
        let tmp = tempfile::TempDir::new().unwrap();
        let (store, default_ws, default_proj, team_ws, team_proj) = scoped_fixture(&tmp).await;

        let active_project = ActiveProject::new();
        let publisher = ActorKey {
            user: Some("alice".into()),
            session_id: Some("s1".into()),
        };
        active_project.set_for(&publisher, team_ws, team_proj, false);
        let stranger = ActorKey {
            user: Some("bob".into()),
            session_id: Some("s9".into()),
        };

        let resolver = ScopeResolver::new(&store.reader, default_ws, default_proj)
            .with_active_project(&active_project);

        let scope = resolver
            .resolve_read_args(None, None, &stranger)
            .await
            .unwrap();
        assert_eq!(scope.as_tuple(), (default_ws, default_proj));
    }

    #[tokio::test]
    async fn the_startup_seed_answers_reads_and_never_retargets_a_write() {
        // #678: after a restart the pointer is empty, so an unscoped read
        // resolved through the baked default and reported an empty project.
        // The seed fixes the read. It must not follow into the write path:
        // `resolve_write_args` still resolves as if nothing were published, so
        // no page is attributed to a project rebuilt from someone else's
        // history.
        let tmp = tempfile::TempDir::new().unwrap();
        let (store, default_ws, default_proj, team_ws, team_proj) = scoped_fixture(&tmp).await;

        let active_project = ActiveProject::new();
        active_project.seed_read_fallback(team_ws, team_proj);

        for actor in [
            // The session that outlived the daemon: coordinate intact, keyed
            // entry gone with the process.
            ActorKey {
                user: Some("alice".into()),
                session_id: Some("s1".into()),
            },
            // And a caller with no coordinate at all.
            ActorKey::default(),
        ] {
            let read = ScopeResolver::new(&store.reader, default_ws, default_proj)
                .with_active_project(&active_project)
                .resolve_read_args(None, None, &actor)
                .await
                .unwrap();
            assert_eq!(
                read.as_tuple(),
                (team_ws, team_proj),
                "read must degrade to the seeded scope, not the empty default"
            );

            let write = ScopeResolver::new(&store.reader, default_ws, default_proj)
                .with_writer(&store.writer)
                .with_active_project(&active_project)
                .resolve_write_args(None, None, &actor)
                .await
                .unwrap();
            assert_eq!(
                write.as_tuple(),
                (default_ws, default_proj),
                "write target must be exactly what it was before the seed existed"
            );
        }

        // A named project still resolves inside the workspace the seed points
        // at — that is a find-only read, and cross-workspace isolation holds:
        // `real-work` exists only in `team`.
        let named = ScopeResolver::new(&store.reader, default_ws, default_proj)
            .with_active_project(&active_project)
            .resolve_read_args(None, Some("real-work"), &ActorKey::default())
            .await
            .unwrap();
        assert_eq!(named.as_tuple(), (team_ws, team_proj));
    }

    #[tokio::test]
    async fn traced_read_resolution_reports_where_the_scope_came_from() {
        // #757: every row is a read that succeeds, so the source is the only
        // way a caller can tell an answer about its own project from one about
        // somebody else's.
        let tmp = tempfile::TempDir::new().unwrap();
        let (store, default_ws, default_proj, team_ws, team_proj) = scoped_fixture(&tmp).await;
        let hook_session = ActorKey {
            user: None,
            session_id: Some("hook-session".into()),
        };
        let static_client = ActorKey {
            user: None,
            session_id: Some("mcp-transport-session".into()),
        };

        let unset = ActiveProject::new();
        let seeded = ActiveProject::new();
        seeded.seed_read_fallback(team_ws, team_proj);
        let live = ActiveProject::new();
        live.set_for(&hook_session, team_ws, team_proj, false);

        struct Case<'a> {
            name: &'static str,
            pointer: Option<&'a ActiveProject>,
            workspace: Option<&'static str>,
            project: Option<&'static str>,
            actor: &'a ActorKey,
            expected: (WorkspaceId, ProjectId),
            source: ScopeSource,
        }
        let cases = [
            Case {
                name: "explicit pair",
                pointer: Some(&live),
                workspace: Some("default"),
                project: Some("scratch"),
                actor: &static_client,
                expected: (default_ws, default_proj),
                source: ScopeSource::Explicit,
            },
            Case {
                name: "project only",
                pointer: Some(&live),
                workspace: None,
                project: Some("real-work"),
                actor: &hook_session,
                expected: (team_ws, team_proj),
                source: ScopeSource::Explicit,
            },
            Case {
                name: "caller's own hook session",
                pointer: Some(&live),
                workspace: None,
                project: None,
                actor: &hook_session,
                expected: (team_ws, team_proj),
                source: ScopeSource::Session,
            },
            Case {
                name: "static client on a live install",
                pointer: Some(&live),
                workspace: None,
                project: None,
                actor: &static_client,
                expected: (default_ws, default_proj),
                source: ScopeSource::DefaultAfterMismatch,
            },
            Case {
                name: "caller with no coordinate",
                pointer: Some(&live),
                workspace: None,
                project: None,
                actor: &ActorKey::default(),
                expected: (team_ws, team_proj),
                source: ScopeSource::SharedSlot,
            },
            Case {
                name: "restart window",
                pointer: Some(&seeded),
                workspace: None,
                project: None,
                actor: &static_client,
                expected: (team_ws, team_proj),
                source: ScopeSource::StartupSeed,
            },
            Case {
                name: "no pointer information",
                pointer: Some(&unset),
                workspace: None,
                project: None,
                actor: &static_client,
                expected: (default_ws, default_proj),
                source: ScopeSource::Default,
            },
            Case {
                name: "no pointer attached",
                pointer: None,
                workspace: None,
                project: None,
                actor: &hook_session,
                expected: (default_ws, default_proj),
                source: ScopeSource::Default,
            },
        ];

        for case in cases {
            let mut resolver = ScopeResolver::new(&store.reader, default_ws, default_proj);
            if let Some(pointer) = case.pointer {
                resolver = resolver.with_active_project(pointer);
            }
            let (scope, source) = resolver
                .resolve_read_args_traced(case.workspace, case.project, case.actor)
                .await
                .unwrap();
            assert_eq!(scope.as_tuple(), case.expected, "{}", case.name);
            assert_eq!(source, case.source, "{}", case.name);
            let untraced = resolver
                .resolve_read_args(case.workspace, case.project, case.actor)
                .await
                .unwrap();
            assert_eq!(
                untraced, scope,
                "{}: tracing must not change the answer",
                case.name
            );
        }

        assert!(ScopeSource::DefaultAfterMismatch.is_fallback());
        assert!(ScopeSource::StartupSeed.is_fallback());
        for honest in [
            ScopeSource::Explicit,
            ScopeSource::Session,
            ScopeSource::SharedSlot,
            ScopeSource::Default,
        ] {
            assert!(!honest.is_fallback(), "{honest}");
        }
    }
}
