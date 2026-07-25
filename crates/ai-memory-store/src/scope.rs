//! Shared workspace/project scope resolution.
//!
//! HTTP admin routes, MCP tools, and the read-only web API all need the same
//! boundary rules: explicit read scopes fail closed, write scopes are the only
//! place that may create projects, and current-project defaults must respect the
//! actor-scoped active-project pointer. Keeping those policies here prevents
//! each surface from growing its own subtly different fallback chain.

use std::collections::HashSet;
use std::fmt;

use ai_memory_core::{ActiveProject, ActorKey, ProjectId, WorkspaceId};

use crate::error::StoreError;
use crate::{ReaderPool, WriterHandle};

/// Canonical error for partial explicit scope arguments.
pub const WORKSPACE_PROJECT_PAIR_REQUIRED: &str = "workspace and project must be provided together";

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
    /// A project name matched multiple workspaces, so an explicit
    /// workspace is required to disambiguate a destructive operation.
    ProjectNameAmbiguousAcrossWorkspaces {
        /// Project name supplied by the caller.
        project: String,
        /// Names of the workspaces that each contain a project with that name.
        workspaces: Vec<String>,
    },
    /// A write-create policy was requested without a writer handle.
    WriterRequired,
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
                | ScopeResolutionError::ProjectNameAmbiguousAcrossWorkspaces { .. }
        )
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
            ScopeResolutionError::ProjectNameAmbiguousAcrossWorkspaces {
                project,
                workspaces,
            } => {
                write!(
                    f,
                    "project '{project}' exists in multiple workspaces ({workspaces:?}); \
                     pass --workspace to disambiguate"
                )
            }
            ScopeResolutionError::WriterRequired => {
                f.write_str("scope resolver requires a writer for create-on-write resolution")
            }
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

/// Look up a workspace/project pair without creating it, falling back to a
/// cross-workspace search for the project name when the explicit pair is absent.
///
/// This preserves the fail-closed behaviour of [`lookup_existing_scope`] for
/// typos, but lets an operator purge a project by its **unique** name without
/// also spelling out the workspace it happens to live in. When the explicit
/// `(workspace, project)` lookup fails, the project name is searched across all
/// workspaces:
///
/// - exactly one match → resolve to that scope (the unique name wins);
/// - more than one match → [`ScopeResolutionError::ProjectNameAmbiguousAcrossWorkspaces`]
///   so the caller requires an explicit `--workspace` (never auto-picks one);
/// - zero matches → the original not-found error is returned unchanged.
///
/// Intended for destructive admin surfaces (e.g. `purge-project`) where a
/// globally-unique project name is unambiguous enough to act on, while
/// ambiguous names still require an explicit scope.
pub async fn lookup_existing_scope_with_global_fallback(
    reader: &ReaderPool,
    workspace: &str,
    project: &str,
) -> Result<ResolvedScope, ScopeResolutionError> {
    match lookup_existing_scope(reader, workspace, project).await {
        Ok(scope) => Ok(scope),
        Err(ScopeResolutionError::WorkspaceNotFound { .. })
        | Err(ScopeResolutionError::ProjectNotFoundInWorkspace { .. }) => {
            let matches = reader.find_project_globally(project.to_owned()).await?;
            match matches.len() {
                1 => {
                    let (project_id, workspace_id, _ws_name) = &matches[0];
                    Ok(ResolvedScope {
                        workspace_id: *workspace_id,
                        project_id: *project_id,
                    })
                }
                0 => Err(ScopeResolutionError::ProjectNotFoundInWorkspace {
                    workspace: workspace.to_owned(),
                    project: project.to_owned(),
                }),
                _ => Err(ScopeResolutionError::ProjectNameAmbiguousAcrossWorkspaces {
                    project: project.to_owned(),
                    workspaces: matches.into_iter().map(|(_, _, wn)| wn).collect(),
                }),
            }
        }
        Err(other) => Err(other),
    }
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

/// Look up the reserved global preferences scope
/// ([`ai_memory_core::GLOBAL_SCOPE_PROJECT`] in the default workspace)
/// without creating it. Returns `Ok(None)` when it doesn't exist yet — the
/// scope participates in default reads by existence, so an absent scope
/// means "nothing to union in", never an error (issue #154).
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
        }
    }

    /// Attach the writer handle needed by create-on-write resolution.
    #[must_use]
    pub fn with_writer(mut self, writer: &'a WriterHandle) -> Self {
        self.writer = Some(writer);
        self
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
        match (
            trimmed_opt(explicit_workspace),
            trimmed_opt(explicit_project),
        ) {
            (Some(workspace), Some(project)) => self.lookup_existing(workspace, project).await,
            (Some(_), None) => Err(ScopeResolutionError::WorkspaceProjectPairRequired),
            (None, project) => self.resolve_current_or_project(project, actor).await,
        }
    }

    /// Resolve a project-only read, or the current/default project when no
    /// project was supplied.
    pub async fn resolve_current_or_project(
        &self,
        explicit_project: Option<&str>,
        actor: &ActorKey,
    ) -> Result<ResolvedScope, ScopeResolutionError> {
        let active = self.active_project.and_then(|a| a.get_for(actor));
        if let Some(project) = trimmed_opt(explicit_project) {
            if let Some((active_ws, _)) = active
                && let Some(project_id) = self
                    .reader
                    .find_project(active_ws, project.to_owned())
                    .await?
            {
                return Ok(ResolvedScope {
                    workspace_id: active_ws,
                    project_id,
                });
            }
            if active.map(|(ws, _)| ws) != Some(self.default_workspace_id)
                && let Some(project_id) = self
                    .reader
                    .find_project(self.default_workspace_id, project.to_owned())
                    .await?
            {
                return Ok(ResolvedScope {
                    workspace_id: self.default_workspace_id,
                    project_id,
                });
            }
            return Err(ScopeResolutionError::ProjectNotFoundInActiveOrDefault {
                project: project.to_owned(),
            });
        }
        let (workspace_id, project_id) =
            active.unwrap_or((self.default_workspace_id, self.default_project_id));
        Ok(ResolvedScope {
            workspace_id,
            project_id,
        })
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
            let active = self.active_project.and_then(|a| a.get_for(actor));
            let (workspace_id, project_id) =
                active.unwrap_or((self.default_workspace_id, self.default_project_id));
            return Ok(ResolvedScope {
                workspace_id,
                project_id,
            });
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
        let project_id = writer
            .get_or_create_project(workspace_id, project.to_owned(), None)
            .await?;
        Ok(ResolvedScope {
            workspace_id,
            project_id,
        })
    }

    /// Resolve and de-duplicate an explicit multi-scope list.
    pub async fn resolve_many_existing(
        &self,
        scopes: &[ScopeName],
        max: usize,
    ) -> Result<Vec<ResolvedScope>, ScopeResolutionError> {
        resolve_many_existing_scopes(self.reader, scopes, max).await
    }
}

fn trimmed_opt(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

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
}
