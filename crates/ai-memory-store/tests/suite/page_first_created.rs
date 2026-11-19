//! `PageMeta::created_at`: the page-level creation instant behind the web
//! header's "Created" and the API's `created_at`.
//!
//! Invariants under test:
//! - **Oldest version wins.** `pages.created_at` is per version, so a page
//!   with several versions reports the first one, not the latest.
//! - **Scoped by `(workspace_id, project_id, path)`.** A same-path page in
//!   another project never moves the date.

use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_store::Store;

fn page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: path.to_string(),
        body: body.into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
        trust_class: None,
    }
}

fn set_created_at(tmp: &tempfile::TempDir, proj: ProjectId, body: &str, us: i64) {
    let conn = rusqlite::Connection::open(tmp.path().join("db/memory.sqlite")).unwrap();
    conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
    let n = conn
        .execute(
            "UPDATE pages SET created_at = ?1 WHERE project_id = ?2 AND body = ?3",
            rusqlite::params![us, proj.as_bytes(), body],
        )
        .unwrap();
    assert_eq!(n, 1, "exactly one version has body {body:?}");
}

#[tokio::test]
async fn first_created_is_the_oldest_version_and_stays_in_its_project() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let app = store
        .writer
        .get_or_create_project(ws, "app".to_string(), None)
        .await
        .unwrap();
    let other = store
        .writer
        .get_or_create_project(ws, "other".to_string(), None)
        .await
        .unwrap();

    for body in ["v1", "v2", "v3"] {
        store
            .writer
            .upsert_page(page(ws, app, "notes/a.md", body))
            .await
            .unwrap();
    }
    // Control: a single-version page reports its own (only) creation time.
    store
        .writer
        .upsert_page(page(ws, app, "notes/single.md", "only"))
        .await
        .unwrap();
    // Same path in another project, older than every `app` version: it must
    // not leak into the `app` answer.
    store
        .writer
        .upsert_page(page(ws, other, "notes/a.md", "foreign"))
        .await
        .unwrap();

    const DAY_US: i64 = 86_400_000_000;
    let now = jiff::Timestamp::now().as_microsecond();
    set_created_at(&tmp, app, "v1", now - 30 * DAY_US);
    set_created_at(&tmp, app, "v2", now - 10 * DAY_US);
    set_created_at(&tmp, app, "v3", now - DAY_US);
    set_created_at(&tmp, other, "foreign", now - 90 * DAY_US);
    set_created_at(&tmp, app, "only", now - 5 * DAY_US);

    let iso = |us: i64| jiff::Timestamp::from_microsecond(us).unwrap().to_string();

    let created = |project: &'static str, path: &'static str| {
        let reader = store.reader.clone();
        async move {
            reader
                .page_meta("default", project, path)
                .await
                .unwrap()
                .map(|m| m.created_at)
        }
    };

    assert_eq!(
        created("app", "notes/a.md").await,
        Some(iso(now - 30 * DAY_US))
    );
    assert_eq!(
        created("app", "notes/single.md").await,
        Some(iso(now - 5 * DAY_US))
    );
    assert_eq!(
        created("other", "notes/a.md").await,
        Some(iso(now - 90 * DAY_US))
    );
    assert_eq!(created("app", "notes/none.md").await, None);
}
