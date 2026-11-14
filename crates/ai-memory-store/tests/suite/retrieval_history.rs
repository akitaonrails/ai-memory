//! Per-session retrieval history (#2038, Workstream 2): records and their hits
//! round-trip, retention keeps the newest-N, a session purge removes them, and —
//! the adversarial half — the history is keyed by the full
//! `(workspace_id, project_id, session_id)` coordinate, so a sibling project or
//! a foreign workspace never reads another scope's rows through the same session
//! id.

use ai_memory_core::{AgentKind, NewSession, ProjectId, SessionId, WorkspaceId};
use ai_memory_store::{NewRetrievalHistory, NewRetrievalHistoryHit, Store};

const SESSION: &str = "11111111-1111-4111-8111-111111111111";

async fn scope(store: &Store, workspace: &str, project: &str) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace(workspace.to_owned())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, project.to_owned(), None)
        .await
        .unwrap();
    (ws, proj)
}

fn record(
    ws: WorkspaceId,
    proj: ProjectId,
    session: SessionId,
    query: &str,
    hits: Vec<NewRetrievalHistoryHit>,
) -> NewRetrievalHistory {
    NewRetrievalHistory {
        workspace_id: ws,
        project_id: proj,
        session_id: session,
        query: query.to_owned(),
        scope_kind: "project".to_owned(),
        source: "fts,entity".to_owned(),
        result_limit: Some(10),
        min_relevance: Some(0.25),
        filters: Some(r#"{"kind":"note"}"#.to_owned()),
        duration_us: 1234,
        result_count: u32::try_from(hits.len()).unwrap(),
        notice: None,
        hits,
    }
}

fn hit(position: u32, path: &str, relevance: Option<f32>) -> NewRetrievalHistoryHit {
    NewRetrievalHistoryHit {
        page_path: path.to_owned(),
        page_version: Some(format!("v-{position}")),
        relevance,
        explanation: None,
    }
}

/// A record and its hits round-trip: every scalar field, and the hits in
/// position order with their scores and version references.
#[tokio::test]
async fn records_round_trip_with_their_hits() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store, "default", "scratch").await;
    let session = SessionId::from_native(SESSION);

    let hits = vec![
        hit(0, "notes/a.md", Some(0.9)),
        hit(1, "notes/b.md", Some(0.4)),
        hit(2, "notes/c.md", None),
    ];
    store
        .writer
        .record_retrieval_history(record(ws, proj, session, "alpha query", hits), 500)
        .await
        .unwrap();

    let page = store
        .reader
        .retrieval_history_page(ws, proj, session, 0, 100)
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    let got = &page.records[0];
    assert_eq!(got.query, "alpha query");
    assert_eq!(got.scope_kind, "project");
    assert_eq!(got.source, "fts,entity");
    assert_eq!(got.result_limit, Some(10));
    assert_eq!(got.min_relevance, Some(0.25));
    assert_eq!(got.filters.as_deref(), Some(r#"{"kind":"note"}"#));
    assert_eq!(got.duration_us, 1234);
    assert_eq!(got.result_count, 3);
    assert_eq!(got.hits.len(), 3);
    assert_eq!(got.hits[0].page_path, "notes/a.md");
    assert_eq!(got.hits[0].relevance, Some(0.9));
    assert_eq!(got.hits[0].page_version.as_deref(), Some("v-0"));
    assert_eq!(got.hits[1].relevance, Some(0.4));
    assert_eq!(
        got.hits[2].relevance, None,
        "an unscored hit stays unscored"
    );
    for (i, h) in got.hits.iter().enumerate() {
        assert_eq!(h.position as usize, i, "hits read back in position order");
    }
}

/// Retention keeps only the newest-N records per session; older ones (and their
/// hits, via cascade) are pruned. The write-path floors a 0 cap to 1.
#[tokio::test]
async fn retention_keeps_the_newest_records() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store, "default", "scratch").await;
    let session = SessionId::from_native(SESSION);

    // Five records, a 2ms gap so `created_at` strictly orders them.
    for i in 0..5 {
        store
            .writer
            .record_retrieval_history(
                record(
                    ws,
                    proj,
                    session,
                    &format!("query-{i}"),
                    vec![hit(0, "notes/a.md", Some(0.5))],
                ),
                2,
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    let page = store
        .reader
        .retrieval_history_page(ws, proj, session, 0, 100)
        .await
        .unwrap();
    assert_eq!(page.total, 2, "retention cap of 2 keeps two records");
    let queries: Vec<&str> = page.records.iter().map(|r| r.query.as_str()).collect();
    assert_eq!(
        queries,
        vec!["query-4", "query-3"],
        "the two newest survive, newest first"
    );
    // The pruned records' hit rows went with them (FK cascade).
    let hit_rows: i64 = store
        .reader
        .retrieval_history_page(ws, proj, session, 0, 100)
        .await
        .unwrap()
        .records
        .iter()
        .map(|r| r.hits.len() as i64)
        .sum();
    assert_eq!(hit_rows, 2, "only the surviving records' hits remain");
}

/// Adversarial isolation: the history is keyed by the full 3-tuple. A sibling
/// project and a foreign workspace, each querying the same session id, read
/// nothing; the real scope reads the record. Dropping the workspace/project
/// predicate from the reader would surface the row and fail this test.
#[tokio::test]
async fn history_is_scoped_by_workspace_and_project() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store, "default", "scratch").await;
    let (other_ws, other_proj) = scope(&store, "other-ws", "other-proj").await;
    let (_, sibling_proj) = scope(&store, "default", "sibling").await;
    let session = SessionId::from_native(SESSION);

    store
        .writer
        .record_retrieval_history(
            record(
                ws,
                proj,
                session,
                "secret query",
                vec![hit(0, "notes/a.md", Some(0.5))],
            ),
            500,
        )
        .await
        .unwrap();

    // Legitimate control.
    let real = store
        .reader
        .retrieval_history_page(ws, proj, session, 0, 100)
        .await
        .unwrap();
    assert_eq!(real.total, 1);

    // A sibling project in the same workspace, same session id: no leak.
    let sibling = store
        .reader
        .retrieval_history_page(ws, sibling_proj, session, 0, 100)
        .await
        .unwrap();
    assert_eq!(sibling.total, 0, "a sibling project reads nothing");

    // A foreign workspace, same session id: no leak.
    let foreign_ws = store
        .reader
        .retrieval_history_page(other_ws, proj, session, 0, 100)
        .await
        .unwrap();
    assert_eq!(foreign_ws.total, 0, "a foreign workspace reads nothing");

    // Guard against a cross-project read even with the foreign project id.
    let foreign_proj = store
        .reader
        .retrieval_history_page(other_ws, other_proj, session, 0, 100)
        .await
        .unwrap();
    assert_eq!(foreign_proj.total, 0);
}

/// A session purge deletes its retrieval history (and the hit rows cascade),
/// scoped to the purged session only.
#[tokio::test]
async fn purge_session_deletes_the_history() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store, "default", "scratch").await;
    let session = SessionId::from_native(SESSION);
    let keep = SessionId::from_native("22222222-2222-4222-8222-222222222222");

    for sid in [session, keep] {
        store
            .writer
            .begin_session(NewSession {
                occurred_at: None,
                id: sid,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
                origin: None,
            })
            .await
            .unwrap();
        store
            .writer
            .record_retrieval_history(
                record(ws, proj, sid, "q", vec![hit(0, "notes/a.md", Some(0.5))]),
                500,
            )
            .await
            .unwrap();
    }

    store
        .writer
        .purge_session(
            ws,
            proj,
            session,
            None,
            ai_memory_store::Compaction::Reclaim,
            ai_memory_store::PurgeMode::Commit,
        )
        .await
        .unwrap();

    let purged = store
        .reader
        .retrieval_history_page(ws, proj, session, 0, 100)
        .await
        .unwrap();
    assert_eq!(purged.total, 0, "the purged session's history is gone");
    let kept = store
        .reader
        .retrieval_history_page(ws, proj, keep, 0, 100)
        .await
        .unwrap();
    assert_eq!(kept.total, 1, "a sibling session's history is untouched");
}

async fn seed_history(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    sid: SessionId,
    query: &str,
    path: &str,
    end: bool,
) {
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: sid,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,
            origin: None,
        })
        .await
        .unwrap();
    if end {
        store.writer.end_session(sid, None).await.unwrap();
    }
    store
        .writer
        .record_retrieval_history(
            record(ws, proj, sid, query, vec![hit(0, path, Some(0.8))]),
            500,
        )
        .await
        .unwrap();
}

/// `move_project_workspace`, `merge_project`, and `move_session` re-stamp
/// `retrieval_history` to the target scope so history and its hits stay
/// readable there and survive deletion of the source scope.
#[tokio::test]
async fn history_follows_project_and_session_through_move_and_merge() {
    let sibling = SessionId::from_native("22222222-2222-4222-8222-222222222222");

    // move_project_workspace: rows move to the destination workspace and
    // survive deleting the vacated workspace.
    {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws1, app) = scope(&store, "default", "app").await;
        let ws2 = store
            .writer
            .get_or_create_workspace("elsewhere".to_owned())
            .await
            .unwrap();
        let sid = SessionId::from_native(SESSION);
        seed_history(&store, ws1, app, sid, "move", "notes/m.md", false).await;

        store
            .writer
            .move_project_workspace(app, ws1, ws2)
            .await
            .unwrap();
        let old = store
            .reader
            .retrieval_history_page(ws1, app, sid, 0, 100)
            .await
            .unwrap();
        assert_eq!(old.total, 0);

        store
            .writer
            .delete_workspace(
                ws1,
                true,
                ai_memory_store::Compaction::Skip,
                ai_memory_store::PurgeMode::Commit,
            )
            .await
            .unwrap();

        let moved = store
            .reader
            .retrieval_history_page(ws2, app, sid, 0, 100)
            .await
            .unwrap();
        assert_eq!(moved.total, 1);
        assert_eq!(moved.records[0].query, "move");
        assert_eq!(moved.records[0].hits.len(), 1);
        assert_eq!(moved.records[0].hits[0].page_path, "notes/m.md");
    }

    // merge_project: source retrieval history is re-keyed to the target and
    // survives source-project deletion alongside existing target history.
    {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, src) = scope(&store, "default", "source").await;
        let dst = store
            .writer
            .get_or_create_project(ws, "target".to_owned(), None)
            .await
            .unwrap();
        let sid = SessionId::from_native(SESSION);
        seed_history(&store, ws, src, sid, "src", "notes/s.md", true).await;
        seed_history(&store, ws, dst, sibling, "dst", "notes/d.md", true).await;

        store
            .writer
            .merge_project(ws, src, dst, Vec::new(), true)
            .await
            .unwrap();

        let merged = store
            .reader
            .retrieval_history_page(ws, dst, sid, 0, 100)
            .await
            .unwrap();
        assert_eq!(merged.total, 1);
        assert_eq!(merged.records[0].query, "src");
        assert_eq!(merged.records[0].hits.len(), 1);
        assert_eq!(merged.records[0].hits[0].page_path, "notes/s.md");

        let untouched = store
            .reader
            .retrieval_history_page(ws, dst, sibling, 0, 100)
            .await
            .unwrap();
        assert_eq!(untouched.total, 1);
    }

    // move_session: only the moved session's history moves to target, leaving
    // sibling sessions untouched and surviving a purge of the source project.
    {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, src) = scope(&store, "default", "source").await;
        let target = store
            .writer
            .get_or_create_project(ws, "target".to_owned(), None)
            .await
            .unwrap();
        let sid = SessionId::from_native(SESSION);
        seed_history(&store, ws, src, sid, "moved", "notes/a.md", true).await;
        seed_history(&store, ws, src, sibling, "stay", "notes/b.md", true).await;

        store
            .writer
            .move_session(
                sid,
                ws,
                target,
                ai_memory_store::PagesMode::Move,
                None,
                true,
            )
            .await
            .unwrap();

        let old = store
            .reader
            .retrieval_history_page(ws, src, sid, 0, 100)
            .await
            .unwrap();
        assert_eq!(old.total, 0);

        let kept = store
            .reader
            .retrieval_history_page(ws, src, sibling, 0, 100)
            .await
            .unwrap();
        assert_eq!(kept.total, 1);

        store
            .writer
            .purge_project(
                ws,
                src,
                "default/source",
                None,
                true,
                ai_memory_store::Compaction::Skip,
                ai_memory_store::PurgeMode::Commit,
            )
            .await
            .unwrap();

        let moved = store
            .reader
            .retrieval_history_page(ws, target, sid, 0, 100)
            .await
            .unwrap();
        assert_eq!(moved.total, 1);
        assert_eq!(moved.records[0].query, "moved");
        assert_eq!(moved.records[0].hits.len(), 1);
        assert_eq!(moved.records[0].hits[0].page_path, "notes/a.md");
    }
}
