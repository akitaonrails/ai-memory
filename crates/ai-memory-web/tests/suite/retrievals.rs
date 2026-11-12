//! The read-only retrieval-history dashboard (#2038, Workstream 2):
//! `GET /w/:ws/:proj/s/:session_id/retrievals`. It renders a session's recorded
//! retrievals, shows an honest empty-state notice, and — the adversarial half —
//! reuses the session-visibility (owner) gate, so one operator cannot read
//! another's session retrievals on a shared server.

use ai_memory_core::{
    ActorContext, AgentKind, AuthLevel, NewSession, ProjectId, SessionId, WorkspaceId,
};
use ai_memory_store::{NewRetrievalHistory, NewRetrievalHistoryHit, Store};
use ai_memory_web::router;
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tempfile::TempDir;
use tower::ServiceExt;

async fn setup() -> (TempDir, Store, Wiki) {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    (tmp, store, wiki)
}

async fn scope(store: &Store) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    (ws, proj)
}

async fn begin(store: &Store, ws: WorkspaceId, proj: ProjectId, owner: Option<&str>) -> SessionId {
    let id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            // Qualified identity storage key, as the hook ingress stores it.
            actor_user: owner.map(|u| format!("user:{u}")),
            origin: None,
        })
        .await
        .unwrap();
    id
}

async fn record(store: &Store, ws: WorkspaceId, proj: ProjectId, session: SessionId, query: &str) {
    store
        .writer
        .record_retrieval_history(
            NewRetrievalHistory {
                workspace_id: ws,
                project_id: proj,
                session_id: session,
                query: query.to_owned(),
                scope_kind: "project".to_owned(),
                source: "fts,entity".to_owned(),
                result_limit: Some(10),
                min_relevance: None,
                filters: None,
                duration_us: 4200,
                result_count: 1,
                notice: None,
                hits: vec![NewRetrievalHistoryHit {
                    page_path: "notes/finding.md".to_owned(),
                    page_version: Some("v1".to_owned()),
                    relevance: Some(0.73),
                    explanation: None,
                }],
            },
            500,
        )
        .await
        .unwrap();
}

async fn body(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// A recorded retrieval renders: the query text, the returned page path, and the
/// relevance score appear on the dashboard.
#[tokio::test]
async fn dashboard_renders_recorded_retrievals() {
    let (_tmp, store, wiki) = setup().await;
    let (ws, proj) = scope(&store).await;
    let session = begin(&store, ws, proj, None).await;
    record(&store, ws, proj, session, "alpha dashboard query").await;

    let app = router(store.reader.clone(), wiki);
    let uri = format!("/w/default/scratch/s/{session}/retrievals");
    let resp = app
        .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = body(resp).await;
    assert!(text.contains("Retrieval history"), "page heading");
    assert!(text.contains("alpha dashboard query"), "query text shown");
    assert!(
        text.contains("notes/finding.md"),
        "returned page path shown"
    );
    assert!(text.contains("0.73"), "relevance score shown");
}

/// A session with no recorded retrievals shows the honest empty-state notice
/// rather than implying it did nothing.
#[tokio::test]
async fn empty_history_shows_the_notice() {
    let (_tmp, store, wiki) = setup().await;
    let (ws, proj) = scope(&store).await;
    let session = begin(&store, ws, proj, None).await;

    let app = router(store.reader.clone(), wiki);
    let uri = format!("/w/default/scratch/s/{session}/retrievals");
    let resp = app
        .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = body(resp).await;
    assert!(
        text.contains("No retrievals recorded for this session"),
        "empty-state notice shown: {text}"
    );
}

/// Adversarial: the dashboard reuses the session-visibility gate. A session
/// owned by `alice` is not visible to `bob`, so `bob` gets a not-found and never
/// sees alice's recorded query; `alice` sees it. Removing the owner gate from
/// the route would leak alice's retrievals to bob and fail this test.
#[tokio::test]
async fn another_operator_cannot_read_a_sessions_retrievals() {
    let (_tmp, store, wiki) = setup().await;
    let (ws, proj) = scope(&store).await;
    let session = begin(&store, ws, proj, Some("alice")).await;
    record(&store, ws, proj, session, "alice private query").await;

    let app = router(store.reader.clone(), wiki);
    let uri = format!("/w/default/scratch/s/{session}/retrievals");
    let request = |user: &str| {
        Request::builder()
            .uri(&uri)
            .extension(ActorContext {
                user: Some(user.to_owned()),
                ..ActorContext::default()
            })
            .extension(AuthLevel::User)
            .body(Body::empty())
            .unwrap()
    };

    // Bob: the session is not his, so the gate hides it — not found, no leak.
    let bob = app.clone().oneshot(request("bob")).await.unwrap();
    assert_eq!(bob.status(), StatusCode::NOT_FOUND);
    let bob_text = body(bob).await;
    assert!(
        !bob_text.contains("alice private query"),
        "another operator's recorded query leaked: {bob_text}"
    );

    // Alice: her own session's retrievals are visible.
    let alice = app.oneshot(request("alice")).await.unwrap();
    assert_eq!(alice.status(), StatusCode::OK);
    let alice_text = body(alice).await;
    assert!(
        alice_text.contains("alice private query"),
        "the owner must see her own retrievals"
    );
}
