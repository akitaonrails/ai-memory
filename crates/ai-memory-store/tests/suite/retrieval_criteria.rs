//! Per-project `[retrieval]` criteria (#2038): the `default_result_limit` and
//! `min_relevance` columns round-trip, default to inherit, clear back to
//! inherit, and are keyed by the full `(workspace_id, project_id)` coordinate.
//! The isolation half is adversarial: one project's stored value must never
//! surface for a sibling project or under a foreign workspace id.

use ai_memory_core::WorkspaceId;
use ai_memory_store::{RetrievalCriteria, Store};

async fn default_workspace(store: &Store) -> WorkspaceId {
    store
        .writer
        .get_or_create_workspace("default".to_owned())
        .await
        .unwrap()
}

/// A project created before these columns existed (or that never set a marker)
/// reads as all-inherit, so the query path falls through to the global
/// default. This is the migration contract: the new columns default to NULL.
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
                ..RetrievalCriteria::default()
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
                ..RetrievalCriteria::default()
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

/// A stored `min_relevance` round-trips independently of the limit, and the two
/// fields persist and clear without disturbing one another — a marker that sets
/// only one leaves the other at its stored value.
#[tokio::test]
async fn min_relevance_persists_and_clears_independently() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = default_workspace(&store).await;
    let proj = store
        .writer
        .get_or_create_project(ws, "work".to_owned(), None)
        .await
        .unwrap();

    // Set both fields.
    store
        .writer
        .set_project_retrieval_criteria(
            proj,
            RetrievalCriteria {
                default_result_limit: Some(15),
                min_relevance: Some(0.4),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .reader
            .project_retrieval_criteria(ws, proj)
            .await
            .unwrap(),
        RetrievalCriteria {
            default_result_limit: Some(15),
            min_relevance: Some(0.4),
        }
    );

    // Clear only the threshold; the limit stays.
    store
        .writer
        .set_project_retrieval_criteria(
            proj,
            RetrievalCriteria {
                default_result_limit: Some(15),
                min_relevance: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .reader
            .project_retrieval_criteria(ws, proj)
            .await
            .unwrap(),
        RetrievalCriteria {
            default_result_limit: Some(15),
            min_relevance: None,
        }
    );
}

/// Adversarial: one project's stored values must not surface for a sibling in
/// the same workspace. A read is keyed by project id, so a sibling that never
/// set a value still inherits.
#[tokio::test]
async fn sibling_project_does_not_inherit_a_neighbor_value() {
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
                min_relevance: Some(0.6),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        store
            .reader
            .project_retrieval_criteria(ws, a)
            .await
            .unwrap(),
        RetrievalCriteria {
            default_result_limit: Some(7),
            min_relevance: Some(0.6),
        }
    );
    // The guard that bites: project B must see nothing, not project A's values.
    assert_eq!(
        store
            .reader
            .project_retrieval_criteria(ws, b)
            .await
            .unwrap(),
        RetrievalCriteria::default(),
        "a sibling project must not inherit another project's retrieval criteria"
    );
}

/// Adversarial: the read is scoped by workspace id too, so a foreign workspace
/// cannot read a project's stored values even with the right project id. This
/// bites if the `workspace_id` filter is dropped from the read query.
#[tokio::test]
async fn foreign_workspace_cannot_read_a_project_value() {
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
                min_relevance: Some(0.5),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        store
            .reader
            .project_retrieval_criteria(ws, proj)
            .await
            .unwrap(),
        RetrievalCriteria {
            default_result_limit: Some(42),
            min_relevance: Some(0.5),
        }
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
        "a foreign workspace must not read a project's retrieval criteria"
    );
}

/// The `CHECK` constraints reject an out-of-range write at the storage
/// boundary, so a forwarded or hand-edited value can never widen or empty a
/// query. The in-range edges (`1`/`100` and `0.0`/`1.0`) persist.
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
                    ..RetrievalCriteria::default()
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

    for relevance in [0.0f32, 1.0] {
        store
            .writer
            .set_project_retrieval_criteria(
                proj,
                RetrievalCriteria {
                    min_relevance: Some(relevance),
                    ..RetrievalCriteria::default()
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
                .min_relevance,
            Some(relevance)
        );
    }
}

/// The `CHECK (min_relevance BETWEEN 0 AND 1)` constraint rejects an
/// out-of-range threshold at the storage boundary. Mirrors the limit guard:
/// a value the forwarding path should already have dropped is refused by SQLite
/// too, so a hand-edited marker cannot store a meaningless floor.
#[tokio::test]
async fn out_of_range_min_relevance_is_rejected_by_the_check() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = default_workspace(&store).await;
    let proj = store
        .writer
        .get_or_create_project(ws, "check".to_owned(), None)
        .await
        .unwrap();

    let err = store
        .writer
        .set_project_retrieval_criteria(
            proj,
            RetrievalCriteria {
                min_relevance: Some(1.5),
                ..RetrievalCriteria::default()
            },
        )
        .await;
    assert!(
        err.is_err(),
        "a min_relevance above 1.0 must be refused by the CHECK constraint"
    );
    // The failed write left the row untouched: still all-inherit.
    assert_eq!(
        store
            .reader
            .project_retrieval_criteria(ws, proj)
            .await
            .unwrap(),
        RetrievalCriteria::default()
    );
}
