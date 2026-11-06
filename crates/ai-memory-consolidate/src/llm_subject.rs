//! What an LLM operation reads and whom it acts for, for the opt-in
//! `openai-compat` request metadata headers (`llm_request_metadata_headers`).
//!
//! Every value is an identifier (a workspace or project name, a username),
//! never page content. Lookups are best effort: a missing name or a failed
//! read leaves the field unset, it never fails the operation it describes.

use ai_memory_core::{IdentityKey, ProjectId, SessionId, WorkspaceId};
use ai_memory_llm::LlmSubject;
use ai_memory_store::ReaderPool;

/// The workspace, and the project when the operation reads exactly one.
pub(crate) async fn scope(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: Option<ProjectId>,
) -> LlmSubject {
    let workspace = reader
        .workspace_name_by_id(workspace_id)
        .await
        .ok()
        .flatten();
    let project = match project_id {
        Some(project_id) => reader
            .project_name_by_id(workspace_id, project_id)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    LlmSubject {
        workspace,
        project,
        user: None,
    }
}

/// The username a session belongs to, as recorded at session start. An OIDC
/// owner without a username has none to send.
pub(crate) async fn session_owner(reader: &ReaderPool, session_id: SessionId) -> Option<String> {
    let owner = reader.session_actor_user(session_id).await.ok().flatten()?;
    username_of(&owner)
}

/// The username in an identity storage key (`user:<name>`), the form session
/// owners and profile contributors are recorded in.
pub(crate) fn username_of(storage_key: &str) -> Option<String> {
    match IdentityKey::from_storage_key(storage_key)? {
        IdentityKey::User(name) => Some(name),
        IdentityKey::Subject { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_username_identity_names_a_user() {
        assert_eq!(username_of("user:alice").as_deref(), Some("alice"));
        assert_eq!(username_of("oidc:19:https://idp.examplesub-1"), None);
        assert_eq!(username_of("user:"), None);
        assert_eq!(username_of("alice"), None);
    }
}
