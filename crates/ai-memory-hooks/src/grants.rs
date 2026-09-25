//! Authorizing hook routes that reach a project through an id (#708).
//!
//! Most routes name a workspace and project and go through the guarded scope
//! resolvers. Some take a managed-run or workstream id in the URL instead,
//! and an id bypasses scope resolution — and the choke point that comes with
//! it — entirely. These resolve the id to its project first, then ask the same
//! question the resolvers ask.

use ai_memory_core::{ProjectId, UserId, WorkspaceId};
use ai_memory_store::{
    ProjectAccess, ReaderPool, ResolvedScope, ScopeResolutionError, authorize_scope_for,
};

/// Authorize `viewer` for `need` on the project an id resolved to.
///
/// `scope` is `None` when the id matched nothing. That passes: the caller goes
/// on to report "not found" exactly as it did before, so an unknown id still
/// reads as unknown rather than as a refusal. A known id in a project the
/// viewer cannot reach is refused with the same `Forbidden` the resolvers
/// return — an access problem, never an empty result.
///
/// No viewer — an install with no database users, or root — passes without
/// a lookup.
pub(crate) async fn authorize_resolved(
    reader: &ReaderPool,
    scope: Option<(WorkspaceId, ProjectId)>,
    viewer: Option<UserId>,
    need: ProjectAccess,
) -> Result<(), ScopeResolutionError> {
    let Some((workspace_id, project_id)) = scope else {
        return Ok(());
    };
    authorize_scope_for(
        reader,
        None,
        ResolvedScope {
            workspace_id,
            project_id,
        },
        viewer,
        need,
    )
    .await
    .map(|_| ())
}
