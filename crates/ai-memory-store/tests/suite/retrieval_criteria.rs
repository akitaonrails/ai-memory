//! Per-project `[retrieval]` criteria (#2038): the `default_result_limit`
//! column round-trips, defaults to inherit, clears back to inherit, and is
//! keyed by the full `(workspace_id, project_id)` coordinate. The isolation
//! half is adversarial: one project's stored default must never surface for a
//! sibling project or under a foreign workspace id.

use ai_memory_core::WorkspaceId;
use ai_memory_store::{RetrievalCriteria, Store};

async fn default_workspace(store: &Store) -> WorkspaceId {
    store
        .writer
        .get_or_create_workspace("default".to_owned())
        .await
        .unwrap()
}

/// A project created before this column existed (or that never set a marker)
/// reads as all-inherit, so the query path falls through to the global
/// default. This is the migration contract: the new column defaults to NULL.
#[tokio::test]
async fn fresh_project_inherits() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = default_workspace(&store).await;
    let proj = store
        .writer
        .get_or_create_project(ws, "fresh".to_owned(), None)
        .await
        .unwrap();
    assert_eq!(
        store
            .reader
            .project_retrieval_criteria(ws, proj)
            .await
            .unwrap(),
        RetrievalCriteria::default()
    );
    assert!(
        store
            .reader
            .project_retrieval_criteria(ws, proj)
            .await
            .unwrap()
            .is_empty()
    );
}

/// A stored limit round-trips, and clearing it (a `None` field) restores the
/// inherit state rather than leaving the last value behind.
#[tokio::test]
async fn limit_persists_and_clears() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = default_workspace(&store).await;
    let proj = store
        .writer
        .get_or_create_project(ws, "work".to_owned(), None)
        .await
        .unwrap();

    store
        .writer
        .set_project_retrieval_criteria(
            proj,
            RetrievalCriteria {
                default_result_limit: Some(25),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .reader
            .project_retrieval_criteria(ws, proj)
            .await
            .unwrap()
            .default_result_limit,
        Some(25)
    );

    // A second identical write is a no-op guarded by `IS NOT`, not an error.
    store
        .writer
        .set_project_retrieval_criteria(
            proj,
            RetrievalCriteria {
                default_result_limit: Some(25),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .reader
            .project_retrieval_criteria(ws, proj)
            .await
            .unwrap()
            .default_result_limit,
        Some(25)
    );

    // Clearing restores inherit.
    store
        .writer
        .set_project_retrieval_criteria(proj, RetrievalCriteria::default())
        .await
        .unwrap();
    assert_eq!(
        store
            .reader
            .project_retrieval_criteria(ws, proj)
            .await
            .unwrap(),
        RetrievalCriteria::default()
    );
}

/// Adversarial: one project's stored limit must not surface for a sibling in
/// the same workspace. A read is keyed by project id, so a sibling that never
/// set a value still inherits.
#[tokio::test]
async fn sibling_project_does_not_inherit_a_neighbor_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = default_workspace(&store).await;
    let a = store
        .writer
        .get_or_create_project(ws, "project-a".to_owned(), None)
        .await
        .unwrap();
    let b = store
        .writer
        .get_or_create_project(ws, "project-b".to_owned(), None)
        .await
        .unwrap();

    store
        .writer
        .set_project_retrieval_criteria(
            a,
            RetrievalCriteria {
                default_result_limit: Some(7),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        store
            .reader
            .project_retrieval_criteria(ws, a)
            .await
            .unwrap()
            .default_result_limit,
        Some(7)
    );
    // The guard that bites: project B must see nothing, not project A's value.
    assert_eq!(
        store
            .reader
            .project_retrieval_criteria(ws, b)
            .await
            .unwrap(),
        RetrievalCriteria::default(),
        "a sibling project must not inherit another project's retrieval limit"
    );
}

/// Adversarial: the read is scoped by workspace id too, so a foreign workspace
/// cannot read a project's stored limit even with the right project id. This
/// bites if the `workspace_id` filter is dropped from the read query.
#[tokio::test]
async fn foreign_workspace_cannot_read_a_project_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = default_workspace(&store).await;
    let other_ws = store
        .writer
        .get_or_create_workspace("other".to_owned())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scoped".to_owned(), None)
        .await
        .unwrap();

    store
        .writer
        .set_project_retrieval_criteria(
            proj,
            RetrievalCriteria {
                default_result_limit: Some(42),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        store
            .reader
            .project_retrieval_criteria(ws, proj)
            .await
            .unwrap()
            .default_result_limit,
        Some(42)
    );
    // Right project id, wrong workspace: the coordinate does not match, so the
    // value is invisible and the caller inherits.
    assert_eq!(
        store
            .reader
            .project_retrieval_criteria(other_ws, proj)
            .await
            .unwrap(),
        RetrievalCriteria::default(),
        "a foreign workspace must not read a project's retrieval limit"
    );
}

/// The `CHECK (default_result_limit BETWEEN 1 AND 100)` constraint rejects an
/// out-of-range write at the storage boundary, so a forwarded or hand-edited
/// value can never widen or empty a query. The in-range `Some(1)`/`Some(100)`
/// edges persist.
#[tokio::test]
async fn range_edges_persist() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = default_workspace(&store).await;
    let proj = store
        .writer
        .get_or_create_project(ws, "edges".to_owned(), None)
        .await
        .unwrap();

    for limit in [1u32, 100] {
        store
            .writer
            .set_project_retrieval_criteria(
                proj,
                RetrievalCriteria {
                    default_result_limit: Some(limit),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .reader
                .project_retrieval_criteria(ws, proj)
                .await
                .unwrap()
                .default_result_limit,
            Some(limit)
        );
    }
}
