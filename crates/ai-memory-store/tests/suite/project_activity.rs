//! Project recency combines capture, session starts, and visible page updates.

use ai_memory_core::{
    AgentKind, NewPage, NewSession, NewUser, PagePath, ProjectId, SessionId, Tier, UserRole,
    WorkspaceId,
};
use ai_memory_store::{AccessMode, GrantLevel, Store};
use rusqlite::{Connection, params};

const BASE: i64 = 1_700_000_000_000_000;

fn timestamp(at: i64) -> String {
    jiff::Timestamp::from_microsecond(at).unwrap().to_string()
}

async fn seed(
    store: &Store,
    ws: WorkspaceId,
    name: &str,
    sessions: &[i64],
    observation: Option<i64>,
    page: Option<i64>,
) -> ProjectId {
    let project = store
        .writer
        .get_or_create_project(ws, name, None)
        .await
        .unwrap();
    let mut session_id = None;
    for &at in sessions {
        let id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                occurred_at: Some(at),
                id,
                workspace_id: ws,
                project_id: project,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        session_id.get_or_insert(id);
    }
    if let Some(at) = observation {
        Connection::open(store.db_path()).unwrap().execute(
            "INSERT INTO observations (id, session_id, workspace_id, project_id, kind, title, body, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'note', 'fixture', 'fixture', ?5)",
            params![ai_memory_core::ObservationId::new().as_bytes(), session_id.unwrap().as_bytes(), ws.as_bytes(), project.as_bytes(), at],
        ).unwrap();
    }
    if let Some(at) = page {
        add_page(store, ws, project, "notes/live.md", at, None).await;
    }
    project
}

async fn add_page(
    store: &Store,
    ws: WorkspaceId,
    project: ProjectId,
    path: &str,
    at: i64,
    expires_at: Option<i64>,
) {
    let id = store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: project,
            path: PagePath::new(path).unwrap(),
            title: "Fixture".into(),
            body: format!("Fixture {at}"),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: expires_at.map(|at| jiff::Timestamp::from_microsecond(at).unwrap()),
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    Connection::open(store.db_path())
        .unwrap()
        .execute(
            "UPDATE pages SET updated_at = ?1 WHERE id = ?2",
            params![at, id.as_bytes()],
        )
        .unwrap();
}

#[tokio::test]
async fn last_activity_chooses_the_latest_source_and_orders_both_project_lists() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace("team").await.unwrap();
    seed(
        &store,
        ws,
        "page-newest",
        &[BASE],
        Some(BASE + 10),
        Some(BASE + 60),
    )
    .await;
    seed(
        &store,
        ws,
        "session-newest",
        &[BASE, BASE + 50],
        Some(BASE + 10),
        Some(BASE + 20),
    )
    .await;
    seed(
        &store,
        ws,
        "observation-newest",
        &[BASE],
        Some(BASE + 40),
        Some(BASE + 20),
    )
    .await;
    seed(&store, ws, "page-only", &[], None, Some(BASE + 30)).await;
    seed(&store, ws, "session-only", &[BASE + 20], None, None).await;
    seed(&store, ws, "before-epoch", &[-30], Some(-20), Some(-10)).await;
    seed(&store, ws, "empty", &[], None, None).await;

    let expected = vec![
        ("page-newest".to_owned(), Some(timestamp(BASE + 60))),
        ("session-newest".to_owned(), Some(timestamp(BASE + 50))),
        ("observation-newest".to_owned(), Some(timestamp(BASE + 40))),
        ("page-only".to_owned(), Some(timestamp(BASE + 30))),
        ("session-only".to_owned(), Some(timestamp(BASE + 20))),
        ("before-epoch".to_owned(), Some(timestamp(-10))),
        ("empty".to_owned(), None),
    ];
    for rows in [
        store.reader.list_projects_with_stats(None).await.unwrap(),
        store
            .reader
            .list_projects_with_stats_for_workspace("team".into(), None)
            .await
            .unwrap(),
    ] {
        let actual: Vec<_> = rows
            .into_iter()
            .map(|row| (row.project_name, row.last_activity))
            .collect();
        assert_eq!(actual, expected);
    }
}

#[tokio::test]
async fn expired_and_superseded_pages_do_not_advance_project_activity() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace("team").await.unwrap();
    let project = seed(
        &store,
        ws,
        "active",
        &[BASE],
        Some(BASE + 30),
        Some(BASE + 20),
    )
    .await;
    add_page(&store, ws, project, "notes/expired.md", BASE + 100, Some(0)).await;
    add_page(&store, ws, project, "notes/history.md", BASE + 90, None).await;
    add_page(&store, ws, project, "notes/history.md", BASE + 10, None).await;
    let empty = seed(&store, ws, "expired-only", &[], None, None).await;
    add_page(&store, ws, empty, "notes/expired.md", BASE + 200, Some(0)).await;
    let rows = store.reader.list_projects_with_stats(None).await.unwrap();
    assert_eq!(rows[0].last_activity, Some(timestamp(BASE + 30)));
    assert_eq!(rows[0].page_count, 2);
    assert_eq!(rows[1].last_activity, None);
    assert_eq!(rows[1].page_count, 0);
}

#[tokio::test]
async fn recent_restricted_activity_stays_hidden_until_the_viewer_has_a_grant() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace("team").await.unwrap();
    seed(&store, ws, "open", &[], None, Some(BASE)).await;
    let private = seed(&store, ws, "private", &[], None, Some(BASE + 100)).await;
    store
        .writer
        .set_access_mode(private, AccessMode::Restricted)
        .await
        .unwrap();
    let viewer = store
        .writer
        .create_human_user(
            NewUser {
                username: "viewer".into(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    for scoped in [false, true] {
        let rows = if scoped {
            store
                .reader
                .list_projects_with_stats_for_workspace("team".into(), Some(viewer))
                .await
                .unwrap()
        } else {
            store
                .reader
                .list_projects_with_stats(Some(viewer))
                .await
                .unwrap()
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].project_name, "open");
        assert_eq!(rows[0].last_activity, Some(timestamp(BASE)));
    }
    store
        .writer
        .grant_memory(viewer, private, GrantLevel::Read, None)
        .await
        .unwrap();
    let rows = store
        .reader
        .list_projects_with_stats(Some(viewer))
        .await
        .unwrap();
    assert_eq!(rows[0].project_name, "private");
    assert_eq!(rows[0].last_activity, Some(timestamp(BASE + 100)));
    assert_eq!(rows.len(), 2);
}
