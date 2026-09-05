//! `derived_index_status` must be able to report FTS drift.
//!
//! `pages_fts` and `observations_fts` are external-content FTS5 tables
//! (`content='pages'` / `content='observations'`), so `SELECT COUNT(*)`
//! against them is answered from the content table and can never diverge from
//! it. Counting the `_docsize` shadow table is what turns the `fts: N/M`
//! status pair into a drift check instead of a tautology.

use ai_memory_core::{
    AgentKind, NewObservation, NewPage, NewSession, ObservationKind, PagePath, Sanitized,
    Sanitizer, SessionId, Tier,
};
use ai_memory_store::Store;

#[tokio::test]
async fn page_fts_rows_report_index_drift() {
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
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/one.md").unwrap(),
            title: "one".into(),
            body: "alpha zebra".into(),
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

    let before = store.reader.derived_index_status().await.unwrap();
    assert_eq!(before.pages_rows, 1);
    assert_eq!(
        before.pages_fts_rows, 1,
        "a freshly written page is indexed",
    );

    // Drop the page's entry from the FTS index while leaving the row in
    // `pages`: exactly the drift this status pair exists to surface.
    {
        let conn = rusqlite::Connection::open(store.db_path()).unwrap();
        conn.execute(
            "INSERT INTO pages_fts(pages_fts, rowid, title, body, path_search) \
             SELECT 'delete', rowid, title, body, path_search FROM pages",
            [],
        )
        .unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH 'alpha'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hits, 0, "the entry really left the index");
    }

    let after = store.reader.derived_index_status().await.unwrap();
    assert_eq!(after.pages_rows, 1, "the source row is untouched");
    assert_eq!(
        after.pages_fts_rows, 0,
        "the status pair must surface the drift, not mirror the content table",
    );
}

#[tokio::test]
async fn observation_fts_rows_report_index_drift() {
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
    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: "bravo zebra".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();

    let before = store.reader.derived_index_status().await.unwrap();
    assert_eq!(before.observations_rows, 1);
    assert_eq!(
        before.observations_fts_rows, 1,
        "a freshly written observation is indexed",
    );

    {
        let conn = rusqlite::Connection::open(store.db_path()).unwrap();
        conn.execute(
            "INSERT INTO observations_fts(observations_fts, rowid, title, body) \
             SELECT 'delete', rowid, title, body FROM observations",
            [],
        )
        .unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM observations_fts WHERE observations_fts MATCH 'bravo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hits, 0, "the entry really left the index");
    }

    let after = store.reader.derived_index_status().await.unwrap();
    assert_eq!(after.observations_rows, 1, "the source row is untouched");
    assert_eq!(
        after.observations_fts_rows, 0,
        "the status pair must surface the drift, not mirror the content table",
    );
}
