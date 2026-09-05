//! Per-operator reinforcement, recorded alongside the shared counter.
//!
//! `pages.access_count` cannot distinguish "50 reads by one person" from "one
//! read by each of 50 people", although only the second says a page is
//! load-bearing for a team. `page_access` records the breakdown WITHOUT
//! replacing the scalar, so the retention formula, the hard-delete predicate
//! and every existing query keep reading exactly what they read before.

use ai_memory_core::{IdentityKey, NewPage, PagePath, Tier};
use ai_memory_store::{DecayParams, Store, retention_score, retention_score_with_breadth};
use rusqlite::{Connection, params};

/// The qualified TEXT the read path records operators under — built through
/// the contract (`IdentityKey::storage_key()`), never hand-written, so these
/// tests break if the storage encoding ever drifts from the API.
fn actor(name: &str) -> IdentityKey {
    IdentityKey::User(name.into())
}

/// Default parameters must reproduce the historical score exactly, whatever the
/// breadth — otherwise adopting the table would silently move every eviction
/// decision on every existing database.
#[test]
fn breadth_is_identity_at_the_default_weight() {
    let params = DecayParams::default();
    let breadth_weight = 0.0;

    for actors in [0, 1, 2, 10, 500] {
        for (age, count, since) in [
            (0.0, 0, None),
            (10.0, 3, Some(2.0)),
            (365.0, 100, Some(200.0)),
        ] {
            assert_eq!(
                retention_score_with_breadth(
                    &params,
                    age,
                    count,
                    since,
                    None,
                    actors,
                    breadth_weight,
                ),
                retention_score(&params, age, count, since, None),
                "default weight must be identity (actors={actors})"
            );
        }
    }
}

/// Even with the weight turned up, 0 and 1 distinct actors score identically to
/// the old formula: a page nobody has read per-actor rows for (everything
/// written before this existed) and a page one person reads are unchanged. That
/// is what removes the eviction cliff and the need for any backfill.
#[test]
fn zero_and_one_actor_score_identically_even_when_weighted() {
    let params = DecayParams::default();
    let breadth_weight = 1.5;
    let baseline = retention_score(&params, 30.0, 5, Some(3.0), None);
    for actors in [0, 1] {
        assert_eq!(
            retention_score_with_breadth(&params, 30.0, 5, Some(3.0), None, actors, breadth_weight,),
            baseline,
            "actors={actors} must not change the score"
        );
    }
    // More readers is worth strictly more, and monotonically so.
    let two = retention_score_with_breadth(&params, 30.0, 5, Some(3.0), None, 2, breadth_weight);
    let ten = retention_score_with_breadth(&params, 30.0, 5, Some(3.0), None, 10, breadth_weight);
    assert!(two > baseline);
    assert!(ten > two);
}

/// A page never accessed scores the same regardless of breadth: with no access
/// timestamp there is no access term to weight.
#[test]
fn never_accessed_pages_are_unaffected_by_breadth() {
    let params = DecayParams::default();
    assert_eq!(
        retention_score_with_breadth(&params, 10.0, 0, None, None, 9, 2.0),
        retention_score(&params, 10.0, 0, None, None)
    );
}

#[test]
fn invalid_breadth_weights_fail_closed_to_the_historical_score() {
    let params = DecayParams::default();
    let baseline = retention_score(&params, 30.0, 5, Some(3.0), None);
    for weight in [-1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            retention_score_with_breadth(&params, 30.0, 5, Some(3.0), None, 50, weight),
            baseline,
        );
    }
}

/// The breakdown is written in the same transaction as the scalar, so the two
/// cannot drift, and an unattributed read still bumps the scalar.
#[tokio::test]
async fn per_actor_rows_accumulate_without_replacing_the_scalar() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "app".to_string(), None)
        .await
        .unwrap();
    let page = store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/x.md").unwrap(),
            title: "x".into(),
            body: "b".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
        })
        .await
        .unwrap();

    store
        .writer
        .bump_access_for_actor(vec![page], Some(actor("alice")))
        .await
        .unwrap();
    store
        .writer
        .bump_access_for_actor(vec![page], Some(actor("alice")))
        .await
        .unwrap();
    store
        .writer
        .bump_access_for_actor(vec![page], Some(actor("bob")))
        .await
        .unwrap();
    // An unattributed read: still counted in the shared scalar.
    store.writer.bump_access(vec![page]).await.unwrap();
    let error = store
        .writer
        .bump_access_for_actor(vec![page], Some(IdentityKey::User(" ".into())))
        .await
        .expect_err("a directly constructed blank identity must be rejected");
    assert!(error.to_string().contains("normalized identity"));

    let conn = Connection::open(store.db_path()).unwrap();
    let scalar: i64 = conn
        .query_row(
            "SELECT access_count FROM pages WHERE id = ?1",
            params![page.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        scalar, 4,
        "the historical counter counts valid reads, not rejected identities"
    );

    let distinct: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM page_access WHERE page_id = ?1",
            params![page.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(distinct, 2, "two named operators, the anonymous read aside");

    let alice_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM page_access WHERE page_id = ?1 AND actor = ?2)",
            params![page.as_bytes(), actor("alice").storage_key()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(alice_exists);
}

/// The bump is fired from a detached task AFTER the search responded, so a page
/// can be deleted in the interval. `page_access.page_id` REFERENCES `pages(id)`
/// with foreign keys ON, so an unguarded insert for that id aborts the
/// transaction and every OTHER page in the same result set silently loses its
/// once-per-window reinforcement — which then makes those pages likelier to be
/// evicted by the next sweep.
#[tokio::test]
async fn a_stale_page_id_does_not_cost_the_rest_of_the_batch_its_bump() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "app".to_string(), None)
        .await
        .unwrap();

    let mut ids = Vec::new();
    for path in ["notes/live-a.md", "notes/doomed.md", "notes/live-b.md"] {
        ids.push(
            store
                .writer
                .upsert_page(NewPage {
                    workspace_id: ws,
                    project_id: proj,
                    path: PagePath::new(path).unwrap(),
                    title: path.into(),
                    body: "b".into(),
                    tier: Tier::Episodic,
                    frontmatter_json: serde_json::json!({}),
                    pinned: false,
                    links: Vec::new(),
                    author_id: None,
                    expires_at: None,
                    entities: Vec::new(),
                })
                .await
                .unwrap(),
        );
    }

    // The page vanishes between the search and the bump.
    store
        .writer
        .delete_page(ws, proj, PagePath::new("notes/doomed.md").unwrap(), None)
        .await
        .unwrap();

    store
        .writer
        .bump_access_for_actor(ids.clone(), Some(actor("alice")))
        .await
        .unwrap();

    let conn = Connection::open(store.db_path()).unwrap();
    for (label, id) in [("live-a", ids[0]), ("live-b", ids[2])] {
        let scalar: i64 = conn
            .query_row(
                "SELECT access_count FROM pages WHERE id = ?1",
                params![id.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(scalar, 1, "{label} lost its bump to the stale sibling");

        let per_actor: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM page_access WHERE page_id = ?1 AND actor = ?2)",
                params![id.as_bytes(), actor("alice").storage_key()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(per_actor, "{label} lost its per-operator row");
    }

    // The unknown id stays a no-op, exactly as it was before per-actor rows.
    let orphaned: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM page_access WHERE page_id = ?1",
            params![ids[1].as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphaned, 0);
}
