//! Integration tests for destructive admin purge endpoints.
//!
//! Follows the same pattern as `admin_phase3.rs`: build a real
//! [`AdminState`] over a tmpdir-backed store + wiki, drive the router
//! with `tower::ServiceExt::oneshot`.

use super::common::{get, post, spawn_capture_hook};
use ai_memory_core::{
    ActorContext, AgentKind, NewHandoff, NewObservation, NewSession, ObservationKind, PagePath,
    ProjectId, Sanitized, Sanitizer, SessionId, Tier, WorkspaceId,
};
use ai_memory_mcp::AdminState;
use ai_memory_store::{DecayParams, PrepareWorkstreamRun, Store, WorkstreamSelection};
use ai_memory_wiki::{
    AdmissionChain, AdmissionOp, FailurePolicy, WebhookConfig, Wiki, WritePageRequest,
};
use axum::Router;
use axum::http::StatusCode;
use axum::routing::post as route_post;
use serde_json::json;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn make_state(tmp: &TempDir) -> (AdminState, Store) {
    make_state_with_chain(tmp, None).await
}

async fn make_state_with_chain(
    tmp: &TempDir,
    chain: Option<AdmissionChain>,
) -> (AdminState, Store) {
    let store = Store::open(tmp.path()).unwrap();
    let mut wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    if let Some(chain) = chain {
        wiki = wiki.with_admission_chain(chain);
    }
    let db_path = store.db_path().to_path_buf();
    let state = AdminState {
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        contradiction_band_min: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
        contradiction_band_max: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
        data_dir: tmp.path().to_path_buf(),
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_gate: ai_memory_mcp::BootstrapGate::default(),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
        db_path,
    };
    (state, store)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn post_purge_session(
    state: AdminState,
    workspace: &str,
    project: &str,
    session_id: SessionId,
) -> axum::response::Response {
    post(
        state,
        "/admin/purge-session",
        json!({
            "workspace": workspace,
            "project": project,
            "session_id": session_id.to_string(),
            "confirm": true,
        }),
    )
    .await
}

async fn seed_ended_session_summary(
    store: &Store,
    wiki: &Wiki,
    workspace: &str,
    project: &str,
    session_id: SessionId,
    body: &str,
) -> (WorkspaceId, ProjectId, PagePath, PathBuf) {
    let ws = store
        .writer
        .get_or_create_workspace(workspace)
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, project, None)
        .await
        .unwrap();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();

    let page_path = PagePath::new(format!("sessions/{session_id}.md")).unwrap();
    let page_id = wiki
        .write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: page_path.clone(),
            frontmatter: json!({"title": "Session summary"}),
            body: body.into(),
            tier: Tier::Episodic,
            pinned: false,
            title: Some("Session summary".into()),
            admission_ctx: None,
            author_id: None,
            actor: ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    store
        .writer
        .end_session(session_id, Some(page_id))
        .await
        .unwrap();

    let summary_file = wiki.abs_path(ws, proj, &page_path);
    (ws, proj, page_path, summary_file)
}

/// Seed two projects (`default/keep` and `default/doomed`), each with one
/// page, one session, some observations, and a handoff. Returns IDs for
/// both projects so callers can construct the per-project wiki paths.
async fn seed_two_projects(store: &Store, wiki: &Wiki) -> (WorkspaceId, ProjectId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let keep = store
        .writer
        .get_or_create_project(ws, "keep", None)
        .await
        .unwrap();
    let doomed = store
        .writer
        .get_or_create_project(ws, "doomed", None)
        .await
        .unwrap();

    // Write pages through the wiki so they land on disk in per-project dirs.
    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: keep,
        path: PagePath::new("notes/keep.md").unwrap(),
        frontmatter: serde_json::json!({"title": "Keep page"}),
        body: "This page must survive the purge.".into(),
        tier: Tier::Semantic,
        pinned: false,
        title: Some("Keep page".into()),
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
        evidence: Vec::new(),
    })
    .await
    .unwrap();

    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: doomed,
        path: PagePath::new("notes/doomed.md").unwrap(),
        frontmatter: serde_json::json!({"title": "Doomed page"}),
        body: "This page will be destroyed.".into(),
        tier: Tier::Semantic,
        pinned: false,
        title: Some("Doomed page".into()),
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
        evidence: Vec::new(),
    })
    .await
    .unwrap();

    // Sessions + observations for both projects.
    for (proj, label) in [(keep, "keep"), (doomed, "doomed")] {
        let sid = SessionId::new();
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
            })
            .await
            .unwrap();
        for i in 0..3u8 {
            store
                .writer
                .insert_observation(Sanitized::new(
                    NewObservation {
                        occurred_at: None,
                        session_id: sid,
                        workspace_id: ws,
                        project_id: proj,
                        kind: ObservationKind::UserPrompt,
                        extension: None,
                        source_event: None,
                        title: format!("{label} obs {i}"),
                        body: "body".into(),
                        importance: 5,
                    },
                    &Sanitizer::builtin(),
                ))
                .await
                .unwrap();
        }
        // Handoff for the doomed project only.
        if label == "doomed" {
            store
                .writer
                .insert_handoff(NewHandoff {
                    workspace_id: ws,
                    project_id: proj,
                    from_session_id: Some(sid),
                    from_agent: AgentKind::ClaudeCode,
                    to_agent: None,
                    cwd: None,
                    summary: "doomed handoff".into(),
                    open_questions: vec![],
                    next_steps: vec![],
                    files_touched: vec![],
                    owner_user: None,
                })
                .await
                .unwrap();
        }
    }

    (ws, keep, doomed)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn purge_session_removes_session_summary_file_from_disk() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let session_id = SessionId::new();
    let (_ws, _proj, page_path, summary_file) = seed_ended_session_summary(
        &store,
        &state.wiki,
        "default",
        "audit",
        session_id,
        "session summary body",
    )
    .await;

    assert!(summary_file.exists(), "setup must create the summary file");

    let resp = post_purge_session(state.clone(), "default", "audit", session_id).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["removed_paths"], json!([page_path.as_str()]));
    assert_eq!(body["files_deleted"], json!([page_path.as_str()]));
    assert_eq!(body["files_failed"], json!([]));
    assert!(
        !summary_file.exists(),
        "purge-session must remove the wiki source file, not only DB rows"
    );

    let read = get(
        state,
        &format!(
            "/admin/read-page?workspace=default&project=audit&path={}",
            page_path.as_str()
        ),
    )
    .await;
    assert_eq!(
        read.status(),
        StatusCode::NOT_FOUND,
        "a purged session page must not remain readable from disk"
    );
}

#[tokio::test]
async fn purge_session_keeps_same_path_in_sibling_project() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let session_id = SessionId::new();
    let (ws, _target, page_path, target_file) = seed_ended_session_summary(
        &store,
        &state.wiki,
        "default",
        "audit",
        session_id,
        "target session summary",
    )
    .await;
    let sibling = store
        .writer
        .get_or_create_project(ws, "keep", None)
        .await
        .unwrap();
    state
        .wiki
        .write_page(WritePageRequest {
            workspace_id: ws,
            project_id: sibling,
            path: page_path.clone(),
            frontmatter: json!({"title": "Sibling summary"}),
            body: "sibling session summary".into(),
            tier: Tier::Episodic,
            pinned: false,
            title: Some("Sibling summary".into()),
            admission_ctx: None,
            author_id: None,
            actor: ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    let sibling_file = state.wiki.abs_path(ws, sibling, &page_path);
    assert!(target_file.exists(), "setup must create target file");
    assert!(sibling_file.exists(), "setup must create sibling file");

    let resp = post_purge_session(state.clone(), "default", "audit", session_id).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!target_file.exists(), "target session file must be removed");
    assert!(
        sibling_file.exists(),
        "same relative path in a sibling project must survive"
    );

    let read = get(
        state,
        &format!(
            "/admin/read-page?workspace=default&project=keep&path={}",
            page_path.as_str()
        ),
    )
    .await;
    assert_eq!(
        read.status(),
        StatusCode::OK,
        "sibling project page must remain readable"
    );
}

/// The SQL purge must commit even when the page file cannot be removed, and
/// async `purge_session` observers must learn that the disk still holds it.
#[tokio::test]
async fn purge_session_reports_file_cleanup_failure_after_db_commit() {
    let (url, rx) = spawn_capture_hook().await;

    let tmp = TempDir::new().unwrap();
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "async-mirror".into(),
        url,
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::PurgeSession],
        blocking: false,
    }])
    .unwrap();
    let (state, store) = make_state_with_chain(&tmp, Some(chain)).await;
    let session_id = SessionId::new();
    let (_ws, _proj, page_path, summary_file) = seed_ended_session_summary(
        &store,
        &state.wiki,
        "default",
        "audit",
        session_id,
        "session summary body",
    )
    .await;
    std::fs::remove_file(&summary_file).unwrap();
    std::fs::create_dir(&summary_file).unwrap();

    let resp = post_purge_session(state, "default", "audit", session_id).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["removed_paths"], json!([page_path.as_str()]));
    assert_eq!(body["files_deleted"], json!([]));
    assert_eq!(body["files_failed"], json!([page_path.as_str()]));
    assert!(
        summary_file.is_dir(),
        "failed cleanup must be reported without pretending the file path was removed"
    );

    let payload = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
        .await
        .expect("async purge_session dispatch should fire")
        .unwrap();
    assert_eq!(payload["ctx"]["op"], "purge_session");
    assert_eq!(payload["ctx"]["workspace"], "default");
    assert_eq!(payload["ctx"]["project"], "audit");
    assert_eq!(payload["ctx"]["partial_failure"], true);
}

/// A preview (no `confirm`, `dry_run: true`) must report the same counts a
/// confirmed purge right after it then actually produces, and must leave the
/// session's page file and DB row untouched.
#[tokio::test]
async fn purge_session_dry_run_reports_the_same_counts_the_confirmed_purge_will() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let session_id = SessionId::new();
    let (ws, proj, page_path, summary_file) = seed_ended_session_summary(
        &store,
        &state.wiki,
        "default",
        "audit",
        session_id,
        "session summary body",
    )
    .await;
    // `seed_ended_session_summary` does not itself insert an observation;
    // add one explicitly so `observations_deleted` has something real to
    // count and this test actually exercises that field.
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
                title: "obs".into(),
                body: "obs body".into(),
                importance: 5,

                occurred_at: None,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();

    let preview = post(
        state.clone(),
        "/admin/purge-session",
        json!({
            "workspace": "default",
            "project": "audit",
            "session_id": session_id.to_string(),
            "confirm": false,
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(preview.status(), StatusCode::OK, "a preview must succeed");
    let preview_body = body_json(preview).await;
    assert_eq!(preview_body["dry_run"], true);
    assert_eq!(preview_body["observations_deleted"], 1);
    assert_eq!(preview_body["pages_deleted"], 1);
    assert_eq!(preview_body["removed_paths"], json!([page_path.as_str()]));
    assert_eq!(preview_body["files_deleted"], json!([]));
    assert_eq!(preview_body["files_failed"], json!([]));
    assert_eq!(preview_body["compacted"], false);
    assert!(
        preview_body.get("pre_checkpoint").is_none(),
        "a preview must not checkpoint the wiki tree"
    );
    assert!(
        preview_body.get("checkpoint").is_none(),
        "a preview must not checkpoint the wiki tree"
    );

    // Nothing was touched: the summary file and the session row survive.
    assert!(
        summary_file.exists(),
        "a preview must not remove the session's page file"
    );

    let confirmed = post_purge_session(state, "default", "audit", session_id).await;
    assert_eq!(confirmed.status(), StatusCode::OK);
    let confirmed_body = body_json(confirmed).await;
    assert_eq!(
        confirmed_body.get("dry_run"),
        None,
        "a confirmed purge report has no dry_run key"
    );
    for field in ["observations_deleted", "pages_deleted"] {
        assert_eq!(
            confirmed_body[field], preview_body[field],
            "the confirmed purge's {field} must match what the preview reported"
        );
    }
    assert!(
        !summary_file.exists(),
        "the confirmed purge must remove the session's page file"
    );
}

/// A dry run must not dispatch the admission webhook: nothing was decided
/// yet, so there is nothing for a mirror to act on.
#[tokio::test]
async fn purge_session_dry_run_does_not_dispatch_admission_webhook() {
    let (url, rx) = spawn_capture_hook().await;

    let tmp = TempDir::new().unwrap();
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "async-mirror".into(),
        url,
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::PurgeSession],
        blocking: false,
    }])
    .unwrap();
    let (state, store) = make_state_with_chain(&tmp, Some(chain)).await;
    let session_id = SessionId::new();
    seed_ended_session_summary(
        &store,
        &state.wiki,
        "default",
        "audit",
        session_id,
        "session summary body",
    )
    .await;

    let resp = post(
        state,
        "/admin/purge-session",
        json!({
            "workspace": "default",
            "project": "audit",
            "session_id": session_id.to_string(),
            "confirm": false,
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let outcome = tokio::time::timeout(std::time::Duration::from_millis(300), rx).await;
    assert!(
        outcome.is_err(),
        "a dry run must not dispatch the purge-session admission webhook"
    );
}

/// A session outside the named scope must still 404 on a preview, exactly
/// like the confirmed purge does.
#[tokio::test]
async fn purge_session_dry_run_nonexistent_returns_404() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    store
        .writer
        .get_or_create_project(ws, "audit", None)
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/purge-session",
        json!({
            "workspace": "default",
            "project": "audit",
            "session_id": SessionId::new().to_string(),
            "confirm": false,
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert!(body["error"].as_str().unwrap_or("").contains("not found"));
}

/// A non-existent workspace must also 404 on a preview.
#[tokio::test]
async fn purge_session_dry_run_nonexistent_workspace_returns_404() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/purge-session",
        json!({
            "workspace": "ghost-workspace",
            "project": "x",
            "session_id": SessionId::new().to_string(),
            "confirm": false,
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Adversarial: the session id is never authority on its own, even at the
/// preview entry point. A session that belongs to project `default/audit`
/// must not be previewable by naming a different, real project in the same
/// workspace, or a different, real workspace whose project happens to share
/// the name `audit` — both must 404, and the 404 body must carry no counts
/// (not a zeroed-out report an operator could mistake for "this scope holds
/// nothing"). Control: previewing under the session's own scope still
/// succeeds and does carry counts.
#[tokio::test]
async fn purge_session_dry_run_refuses_a_session_outside_its_named_scope() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let session_id = SessionId::new();
    seed_ended_session_summary(
        &store,
        &state.wiki,
        "default",
        "audit",
        session_id,
        "session summary body",
    )
    .await;
    // A sibling project in the same workspace as the session.
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    store
        .writer
        .get_or_create_project(ws, "other-project", None)
        .await
        .unwrap();
    // A different workspace whose project happens to share the name "audit".
    let other_ws = store
        .writer
        .get_or_create_workspace("other-workspace")
        .await
        .unwrap();
    store
        .writer
        .get_or_create_project(other_ws, "audit", None)
        .await
        .unwrap();

    let wrong_project = post(
        state.clone(),
        "/admin/purge-session",
        json!({
            "workspace": "default",
            "project": "other-project",
            "session_id": session_id.to_string(),
            "confirm": false,
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(
        wrong_project.status(),
        StatusCode::NOT_FOUND,
        "a real project that does not hold this session must still 404"
    );
    let wrong_project_body = body_json(wrong_project).await;
    assert!(
        wrong_project_body.get("observations_deleted").is_none()
            && wrong_project_body.get("pages_deleted").is_none(),
        "a 404 must carry no counts: {wrong_project_body}"
    );

    let wrong_workspace = post(
        state.clone(),
        "/admin/purge-session",
        json!({
            "workspace": "other-workspace",
            "project": "audit",
            "session_id": session_id.to_string(),
            "confirm": false,
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(
        wrong_workspace.status(),
        StatusCode::NOT_FOUND,
        "a same-named project in a different, real workspace must still 404"
    );
    let wrong_workspace_body = body_json(wrong_workspace).await;
    assert!(
        wrong_workspace_body.get("observations_deleted").is_none()
            && wrong_workspace_body.get("pages_deleted").is_none(),
        "a 404 must carry no counts: {wrong_workspace_body}"
    );

    // Control: the session's own scope still previews fine, and does report
    // counts — proving the two 404s above are the scope check, not a broken
    // route.
    let control = post(
        state,
        "/admin/purge-session",
        json!({
            "workspace": "default",
            "project": "audit",
            "session_id": session_id.to_string(),
            "confirm": false,
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(
        control.status(),
        StatusCode::OK,
        "the session's own scope must still preview"
    );
    let control_body = body_json(control).await;
    assert_eq!(control_body["dry_run"], true);
    assert!(control_body.get("pages_deleted").is_some());
}

/// Mirrors `purge_project_confirm_true_and_dry_run_true_still_only_previews`:
/// `dry_run` must always win over `confirm`. `{"confirm": true, "dry_run":
/// true}` must never run the real destructive purge.
#[tokio::test]
async fn purge_session_confirm_true_and_dry_run_true_still_only_previews() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let session_id = SessionId::new();
    let (ws, proj, _page_path, summary_file) = seed_ended_session_summary(
        &store,
        &state.wiki,
        "default",
        "audit",
        session_id,
        "session summary body",
    )
    .await;
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
                title: "obs".into(),
                body: "obs body".into(),
                importance: 5,

                occurred_at: None,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/purge-session",
        json!({
            "workspace": "default",
            "project": "audit",
            "session_id": session_id.to_string(),
            "confirm": true,
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["dry_run"], true,
        "confirm: true must not defeat dry_run: true"
    );
    assert_eq!(body["observations_deleted"], 1);
    assert_eq!(body["pages_deleted"], 1);

    // The only proof that matters: nothing was actually deleted. Checking
    // only the page file would still pass if the handler passed
    // `PurgeMode::Commit` to the writer instead of `Preview` — the DB rows
    // are what a real purge actually removes, so check those too.
    assert!(
        summary_file.exists(),
        "{{confirm: true, dry_run: true}} must not delete the session's page file"
    );
    assert!(
        store
            .reader
            .find_session_scope(session_id)
            .await
            .unwrap()
            .is_some(),
        "{{confirm: true, dry_run: true}} must not delete the session row"
    );
    assert_eq!(
        store.reader.status_counts().await.unwrap().observations,
        1,
        "{{confirm: true, dry_run: true}} must not delete the session's observation"
    );
}

/// The mirror of the incident `purge-project`'s preview guards against, one
/// level down at session granularity: purging a session also collaterally
/// deletes an observation stamped into a *different* project (because
/// `observations.session_id` cascades regardless of the observation's own
/// `project_id`), and orphans (nulls the session reference of, without
/// deleting) a handoff that lives in that other project too.
#[tokio::test]
async fn purge_session_dry_run_and_confirmed_purge_both_report_collateral_damage_in_another_project()
 {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let owner = store
        .writer
        .get_or_create_project(ws, "owner", None)
        .await
        .unwrap();
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();

    let sid = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid,
            workspace_id: ws,
            project_id: owner,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,

            occurred_at: None,
        })
        .await
        .unwrap();
    // Collateral observation: session lives in `owner`, observation is
    // stamped into `other`.
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id: sid,
                workspace_id: ws,
                project_id: other,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "collateral".into(),
                body: "collateral body".into(),
                importance: 5,

                occurred_at: None,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();
    // Collateral handoff: lives in `other`, authored by the `owner` session.
    store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: other,
            from_session_id: Some(sid),
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "collateral handoff".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        })
        .await
        .unwrap();

    let preview = post(
        state.clone(),
        "/admin/purge-session",
        json!({
            "workspace": "default",
            "project": "owner",
            "session_id": sid.to_string(),
            "confirm": false,
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(preview.status(), StatusCode::OK);
    let preview_body = body_json(preview).await;
    assert_eq!(preview_body["collateral_observations_deleted"], 1);
    assert_eq!(preview_body["collateral_handoffs_denulled"], 1);

    let confirmed = post(
        state,
        "/admin/purge-session",
        json!({
            "workspace": "default",
            "project": "owner",
            "session_id": sid.to_string(),
            "confirm": true
        }),
    )
    .await;
    assert_eq!(confirmed.status(), StatusCode::OK);
    let confirmed_body = body_json(confirmed).await;
    assert_eq!(confirmed_body["collateral_observations_deleted"], 1);
    assert_eq!(confirmed_body["collateral_handoffs_denulled"], 1);

    // `other` survives as a project; only the collateral rows are affected.
    assert_eq!(
        store.reader.status_counts().await.unwrap().observations,
        0,
        "the collateral observation in `other` must actually be gone"
    );
}

/// Missing `confirm: true` must return 400.
#[tokio::test]
async fn purge_project_without_confirm_returns_400() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "any", "confirm": false }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body = body_json(resp).await;
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("confirm=true"),
        "error must mention confirm=true: {body}"
    );
}

/// Omitting `confirm` entirely is the same refusal as `confirm: false`: the
/// documented 400 that names the flag, not the JSON extractor's bare 422 that
/// operators read as a malformed request. Every route whose docs promise that
/// 400 is listed here.
#[tokio::test]
async fn omitted_confirm_is_the_documented_400_on_every_guarded_route() {
    let cases = [
        (
            "/admin/handoffs/expire",
            json!({ "workspace": "default", "project": "any" }),
        ),
        (
            "/admin/purge-session",
            json!({
                "workspace": "default",
                "project": "any",
                "session_id": "00000000-0000-0000-0000-000000000000",
            }),
        ),
        (
            "/admin/purge-project",
            json!({ "workspace": "default", "project": "any" }),
        ),
        ("/admin/compact", json!({})),
        ("/admin/reclaim-ledger-versions", json!({})),
        (
            "/admin/move-project",
            json!({ "from_workspace": "default", "project": "any", "to_workspace": "other" }),
        ),
    ];
    for (uri, body) in cases {
        let tmp = TempDir::new().unwrap();
        let (state, _store) = make_state(&tmp).await;

        let resp = post(state, uri, body).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{uri}");

        let body = body_json(resp).await;
        assert!(
            body["error"].as_str().unwrap_or("").contains("confirm"),
            "{uri}: the refusal must name the flag: {body}"
        );
    }
}

/// Non-existent project must return 404.
#[tokio::test]
async fn purge_project_nonexistent_returns_404() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    // Ensure the workspace exists so the 404 is about the project.
    store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "nonexistent", "confirm": true }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "nonexistent project must 404"
    );

    let body = body_json(resp).await;
    assert!(
        body["error"].as_str().unwrap_or("").contains("not found"),
        "error must say 'not found': {body}"
    );
}

/// Non-existent workspace must also return 404.
#[tokio::test]
async fn purge_project_nonexistent_workspace_returns_404() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "ghost-workspace", "project": "x", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Happy path: purge the doomed project, verify counts and directory removal.
#[tokio::test]
async fn purge_project_deletes_data_and_files() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let (ws, _keep, doomed) = seed_two_projects(&store, &state.wiki).await;

    // The per-project directory must exist before purge.
    let proj_dir = state.wiki.project_root(ws, doomed);
    assert!(proj_dir.exists(), "project dir must exist before purge");

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "purge must succeed");

    let body = body_json(resp).await;
    assert_eq!(
        body["label"].as_str().unwrap_or(""),
        "default/doomed",
        "label must match: {body}"
    );
    assert_eq!(
        body["pages_deleted"].as_u64().unwrap_or(0),
        1,
        "one page deleted: {body}"
    );
    assert_eq!(
        body["sessions_deleted"].as_u64().unwrap_or(0),
        1,
        "one session deleted: {body}"
    );
    assert_eq!(
        body["observations_deleted"].as_u64().unwrap_or(0),
        3,
        "three observations deleted: {body}"
    );
    assert_eq!(
        body["handoffs_deleted"].as_u64().unwrap_or(0),
        1,
        "one handoff deleted: {body}"
    );

    // The entire project directory must be gone.
    assert!(
        !proj_dir.exists(),
        "project directory must be removed after purge"
    );
}

#[tokio::test]
async fn purge_project_removes_raw_workstream_segments() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let (workspace_id, _keep, project_id) = seed_two_projects(&store, &state.wiki).await;
    let prepared = store
        .writer
        .prepare_workstream_run(PrepareWorkstreamRun {
            scope_by_id: false,
            workspace_id,
            project_id,
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            cwd: "/repo".into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: vec![AgentKind::Codex],
            selection: WorkstreamSelection::Current,
            lease_owner: "test".into(),
        })
        .await
        .unwrap();
    assert!(
        store
            .writer
            .cancel_managed_run(prepared.run_id)
            .await
            .unwrap()
    );
    let raw_dir = tmp
        .path()
        .join("raw/workstreams")
        .join(prepared.workstream_id.to_string());
    std::fs::create_dir_all(&raw_dir).unwrap();
    std::fs::write(raw_dir.join("000001.jsonl"), "event\n").unwrap();

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    assert!(
        !raw_dir.exists(),
        "raw workstream directory must be removed"
    );
    assert_eq!(body["workstreams_deleted"], 1);
    assert_eq!(body["managed_runs_deleted"], 1);
    assert_eq!(
        body["workstream_ids"],
        json!([prepared.workstream_id.to_string()])
    );
    let raw_suffix = Path::new("raw")
        .join("workstreams")
        .join(prepared.workstream_id.to_string());
    assert!(
        body["files_deleted"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|path| path.as_str())
            .any(|path| Path::new(path).ends_with(&raw_suffix)),
        "raw cleanup must be visible in the report: {body}"
    );
}

/// A reject-policy purge webhook must be able to abort before DB rows or files
/// are deleted. This guards the destructive-operation ordering.
#[tokio::test]
async fn purge_project_rejecting_admission_leaves_source_intact() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    let app = Router::new().route(
        "/guard",
        route_post(|| async { (StatusCode::FORBIDDEN, "blocked") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "purge-guard".into(),
        url: format!("http://{addr}/guard"),
        timeout_ms: 1_000,
        failure_policy: FailurePolicy::Reject,
        events: vec![AdmissionOp::PurgeProject],
        blocking: true,
    }])
    .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_admission_chain(chain)
        .with_store_reader(store.reader.clone());
    let state = AdminState {
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        contradiction_band_min: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
        contradiction_band_max: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_gate: ai_memory_mcp::BootstrapGate::default(),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    };

    let (ws, _keep, doomed) = seed_two_projects(&store, &state.wiki).await;
    let doomed_dir = state.wiki.project_root(ws, doomed);

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

    assert!(
        store
            .reader
            .find_project(ws, "doomed".to_string())
            .await
            .unwrap()
            .is_some(),
        "rejecting admission must leave project row intact"
    );
    assert!(
        doomed_dir.exists(),
        "rejecting admission must leave files intact"
    );
    assert_eq!(
        store
            .reader
            .list_pages("default", "doomed")
            .await
            .unwrap()
            .len(),
        1
    );
}

/// After purging `doomed`, `keep` must still have all its data and files intact.
#[tokio::test]
async fn purge_project_preserves_sibling_project() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let (ws, keep, _doomed) = seed_two_projects(&store, &state.wiki).await;
    // Clone the wiki handle before state is consumed by `post`.
    let wiki = state.wiki.clone();

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The `keep` project's data must be untouched.
    let counts = store.reader.status_counts().await.unwrap();
    // 1 page (keep), 1 session (keep), 3 observations (keep).
    assert_eq!(
        counts.pages_latest, 1,
        "keep's page must survive: {counts:?}"
    );
    assert_eq!(
        counts.sessions, 1,
        "keep's session must survive: {counts:?}"
    );
    assert_eq!(
        counts.observations, 3,
        "keep's observations must survive: {counts:?}"
    );

    // keep's wiki directory must still exist with its page.
    let keep_dir = wiki.project_root(ws, keep);
    assert!(keep_dir.exists(), "keep's project dir must survive");
    let keep_file = keep_dir.join("notes/keep.md");
    assert!(keep_file.exists(), "keep's wiki file must survive");
}

/// Purging the same project twice: second call returns 404 (already gone).
#[tokio::test]
async fn purge_project_idempotent_second_call_is_404() {
    let tmp = TempDir::new().unwrap();
    let (state_a, store) = make_state(&tmp).await;
    let state_b = AdminState {
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki: state_a.wiki.clone(),
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        contradiction_band_min: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
        contradiction_band_max: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_gate: ai_memory_mcp::BootstrapGate::default(),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    };

    seed_two_projects(&store, &state_a.wiki).await;

    // First purge succeeds.
    let r1 = post(
        state_a,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": true }),
    )
    .await;
    assert_eq!(r1.status(), StatusCode::OK);

    // Second purge: project is gone → 404.
    let r2 = post(
        state_b,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": true }),
    )
    .await;
    assert_eq!(
        r2.status(),
        StatusCode::NOT_FOUND,
        "second purge must 404 because project is already gone"
    );
}

// ---------------------------------------------------------------------------
// Dry-run preview (no `confirm`, `dry_run: true`)
// ---------------------------------------------------------------------------

/// The preview must report the exact counts a confirmed purge would produce
/// — not an estimate — and it must not touch anything: the confirmed purge
/// run right after it must succeed with identical counts, and the project's
/// files and DB rows must still be intact in between.
#[tokio::test]
async fn purge_project_dry_run_reports_the_same_counts_the_confirmed_purge_will() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let (ws, _keep, doomed) = seed_two_projects(&store, &state.wiki).await;
    let proj_dir = state.wiki.project_root(ws, doomed);

    let preview = post(
        state.clone(),
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": false, "dry_run": true }),
    )
    .await;
    assert_eq!(preview.status(), StatusCode::OK, "a preview must succeed");
    let preview_body = body_json(preview).await;
    assert_eq!(preview_body["dry_run"], true);
    assert_eq!(preview_body["pages_deleted"], 1);
    assert_eq!(preview_body["sessions_deleted"], 1);
    assert_eq!(preview_body["observations_deleted"], 3);
    assert_eq!(preview_body["handoffs_deleted"], 1);
    assert_eq!(preview_body["files_deleted"], json!([]));
    assert_eq!(preview_body["files_failed"], json!([]));
    assert_eq!(preview_body["compacted"], false);
    assert!(
        preview_body.get("pre_checkpoint").is_none(),
        "a preview must not checkpoint the wiki tree"
    );
    assert!(
        preview_body.get("checkpoint").is_none(),
        "a preview must not checkpoint the wiki tree"
    );

    // Nothing was touched: the project directory and its row are still there.
    assert!(
        proj_dir.exists(),
        "a preview must not remove the project directory"
    );
    assert!(
        store
            .reader
            .find_project(ws, "doomed".to_string())
            .await
            .unwrap()
            .is_some(),
        "a preview must not delete the project row"
    );

    // The confirmed purge right after it must succeed with the same counts.
    let confirmed = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": true }),
    )
    .await;
    assert_eq!(confirmed.status(), StatusCode::OK);
    let confirmed_body = body_json(confirmed).await;
    assert_eq!(
        confirmed_body.get("dry_run"),
        None,
        "a confirmed purge report has no dry_run key"
    );
    for field in [
        "pages_deleted",
        "sessions_deleted",
        "observations_deleted",
        "handoffs_deleted",
    ] {
        assert_eq!(
            confirmed_body[field], preview_body[field],
            "the confirmed purge's {field} must match what the preview reported"
        );
    }
    assert!(
        !proj_dir.exists(),
        "the confirmed purge must remove the project directory"
    );
}

/// A dry run must not dispatch the admission webhook: nothing was decided
/// yet, so there is nothing for a mirror to act on. Uses the same
/// `Ignore`/non-blocking capture-hook pattern as
/// `purge_session_reports_file_cleanup_failure_after_db_commit` above, but
/// asserts the opposite — that no payload ever arrives — within a short
/// timeout.
#[tokio::test]
async fn purge_project_dry_run_does_not_dispatch_admission_webhook() {
    let (url, rx) = spawn_capture_hook().await;

    let tmp = TempDir::new().unwrap();
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "async-mirror".into(),
        url,
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::PurgeProject],
        blocking: false,
    }])
    .unwrap();
    let (state, store) = make_state_with_chain(&tmp, Some(chain)).await;
    seed_two_projects(&store, &state.wiki).await;

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": false, "dry_run": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let outcome = tokio::time::timeout(std::time::Duration::from_millis(300), rx).await;
    assert!(
        outcome.is_err(),
        "a dry run must not dispatch the purge-project admission webhook"
    );
}

/// A non-existent project must still 404 on a preview, exactly like the
/// confirmed purge does.
#[tokio::test]
async fn purge_project_dry_run_nonexistent_returns_404() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/purge-project",
        json!({
            "workspace": "default",
            "project": "nonexistent",
            "confirm": false,
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert!(body["error"].as_str().unwrap_or("").contains("not found"));
}

/// A non-existent workspace must also 404 on a preview.
#[tokio::test]
async fn purge_project_dry_run_nonexistent_workspace_returns_404() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/purge-project",
        json!({
            "workspace": "ghost-workspace",
            "project": "x",
            "confirm": false,
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// The incident this feature guards against, exercised through the HTTP
/// route rather than the store function directly: a session lives in one
/// project but one of its observations is stamped into another (the
/// pre-#871 Windows path-casing split). The preview of the *other* project
/// must count that observation even though no session row lives there —
/// exactly the number a naive "0 sessions, 0 pages" glance would miss.
#[tokio::test]
async fn purge_project_dry_run_counts_observations_stamped_from_another_projects_session() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let owner = store
        .writer
        .get_or_create_project(ws, "owner", None)
        .await
        .unwrap();
    let doomed = store
        .writer
        .get_or_create_project(ws, "looks-empty", None)
        .await
        .unwrap();

    let sid = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid,
            workspace_id: ws,
            project_id: owner,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,

            occurred_at: None,
        })
        .await
        .unwrap();
    // The stray observation: same session, different project_id.
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id: sid,
                workspace_id: ws,
                project_id: doomed,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "stray".into(),
                body: "stray body".into(),
                importance: 5,

                occurred_at: None,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/purge-project",
        json!({
            "workspace": "default",
            "project": "looks-empty",
            "confirm": false,
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["sessions_deleted"], 0,
        "the session row itself lives in the owner project"
    );
    assert_eq!(
        body["observations_deleted"], 1,
        "the stray observation stamped into the previewed project must be counted"
    );
}

/// BLOCKING fix: `dry_run` must always win over `confirm`. Before this test
/// existed, `{"confirm": true, "dry_run": true}` ran the real destructive
/// purge — `handle_purge_project` only checked `dry_run` inside the
/// `!confirm` branch, so a caller that (accidentally or not) sent both
/// `true` got the worst of both: a request that reads like a preview and
/// behaves like a purge. `reclaim-ledger-versions` never had this hole
/// because its handler passes `req.dry_run` straight into the op regardless
/// of `confirm`; `purge-project` now checks `dry_run` first, unconditionally,
/// exactly the same way.
#[tokio::test]
async fn purge_project_confirm_true_and_dry_run_true_still_only_previews() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let (ws, _keep, doomed) = seed_two_projects(&store, &state.wiki).await;
    let proj_dir = state.wiki.project_root(ws, doomed);

    let resp = post(
        state,
        "/admin/purge-project",
        json!({
            "workspace": "default",
            "project": "doomed",
            "confirm": true,
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["dry_run"], true,
        "confirm: true must not defeat dry_run: true"
    );
    assert_eq!(body["pages_deleted"], 1);
    assert_eq!(body["sessions_deleted"], 1);
    assert_eq!(body["observations_deleted"], 3);

    // The only proof that matters: nothing was actually deleted.
    assert!(
        proj_dir.exists(),
        "{{confirm: true, dry_run: true}} must not delete the project directory"
    );
    assert!(
        store
            .reader
            .find_project(ws, "doomed".to_string())
            .await
            .unwrap()
            .is_some(),
        "{{confirm: true, dry_run: true}} must not delete the project row"
    );
}

/// The mirror of the incident, exercised through the HTTP route: purging a
/// project P also collaterally deletes an observation stamped into a
/// *different* project Q (because `observations.session_id` cascades
/// regardless of the observation's own `project_id`), and orphans (nulls the
/// session reference of, without deleting) a handoff that lives in Q too.
/// Neither shows up in the plain `observations_deleted`/`handoffs_deleted`
/// counts, which is exactly why the two `collateral_*` fields exist.
#[tokio::test]
async fn purge_project_dry_run_and_confirmed_purge_both_report_collateral_damage_in_another_project()
 {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let doomed = store
        .writer
        .get_or_create_project(ws, "doomed", None)
        .await
        .unwrap();
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();

    let sid = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid,
            workspace_id: ws,
            project_id: doomed,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,

            occurred_at: None,
        })
        .await
        .unwrap();
    // Collateral observation: session lives in `doomed`, observation is
    // stamped into `other`.
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id: sid,
                workspace_id: ws,
                project_id: other,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "collateral".into(),
                body: "collateral body".into(),
                importance: 5,

                occurred_at: None,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();
    // Collateral handoff: lives in `other`, authored by the `doomed` session.
    store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: other,
            from_session_id: Some(sid),
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "collateral handoff".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        })
        .await
        .unwrap();

    let preview = post(
        state.clone(),
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": false, "dry_run": true }),
    )
    .await;
    assert_eq!(preview.status(), StatusCode::OK);
    let preview_body = body_json(preview).await;
    assert_eq!(preview_body["collateral_observations_deleted"], 1);
    assert_eq!(preview_body["collateral_handoffs_denulled"], 1);

    let confirmed = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": true }),
    )
    .await;
    assert_eq!(confirmed.status(), StatusCode::OK);
    let confirmed_body = body_json(confirmed).await;
    assert_eq!(confirmed_body["collateral_observations_deleted"], 1);
    assert_eq!(confirmed_body["collateral_handoffs_denulled"], 1);

    // `other` survives as a project; only the collateral rows are affected.
    assert_eq!(
        store.reader.status_counts().await.unwrap().observations,
        0,
        "the collateral observation in `other` must actually be gone"
    );
}

/// A live managed-run lease under the project must still refuse the preview
/// with the same `409` a confirmed purge would, unless `force` overrides it
/// — the preview promises to describe what a confirmed call would do, and a
/// confirmed call would refuse here too.
#[tokio::test]
async fn purge_project_dry_run_conflicts_on_a_live_managed_run_without_force() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let (workspace_id, _keep, project_id) = seed_two_projects(&store, &state.wiki).await;
    store
        .writer
        .prepare_workstream_run(PrepareWorkstreamRun {
            scope_by_id: false,
            workspace_id,
            project_id,
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            cwd: "/repo".into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: vec![AgentKind::Codex],
            selection: WorkstreamSelection::Current,
            lease_owner: "test".into(),
        })
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": false, "dry_run": true }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "a live managed run must still 409 a preview without --force"
    );
}

// ---------------------------------------------------------------------------
// #1208: `project_id` as an alternative to `project` / `from`
// ---------------------------------------------------------------------------

/// `purge-project` accepts `project_id` in place of `project`, so an operator
/// can target a legacy twin row whose NAME collides with another project's
/// `legacy_name` (the name lookup resolves to the wrong row in that case).
#[tokio::test]
async fn purge_project_accepts_project_id_instead_of_name() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let (ws, _keep, doomed) = seed_two_projects(&store, &state.wiki).await;

    let resp = post(
        state,
        "/admin/purge-project",
        json!({
            "workspace": "default",
            "project_id": doomed.to_string(),
            "confirm": true,
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "purge by project_id must resolve the same target as purge by name"
    );

    assert!(
        store
            .reader
            .find_project(ws, "doomed".to_string())
            .await
            .unwrap()
            .is_none(),
        "the row named by project_id must be the one purged"
    );
}

/// A `project_id` that is not a UUID is a client error (400), not a 404 or a
/// 500: it can never match a row, so the request itself is malformed.
#[tokio::test]
async fn purge_project_rejects_a_malformed_project_id_with_400() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/purge-project",
        json!({
            "workspace": "default",
            "project_id": "not-a-uuid",
            "confirm": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// A well-formed `project_id` with no matching row in this workspace is a 404.
/// A foreign-scope id lands here too: the row is not visible from this
/// workspace, so it must never be purged by a stray id.
#[tokio::test]
async fn purge_project_unknown_project_id_is_404() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/purge-project",
        json!({
            "workspace": "default",
            "project_id": "00000000-0000-0000-0000-0000000000ff",
            "confirm": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Sending BOTH `project` and `project_id` is ambiguous, so it is refused
/// before any lookup runs rather than silently preferring one.
#[tokio::test]
async fn purge_project_refuses_both_project_and_project_id() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let (_ws, _keep, doomed) = seed_two_projects(&store, &state.wiki).await;

    let resp = post(
        state,
        "/admin/purge-project",
        json!({
            "workspace": "default",
            "project": "doomed",
            "project_id": doomed.to_string(),
            "confirm": true,
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "project and project_id are mutually exclusive"
    );

    assert!(
        store
            .reader
            .find_project(_ws, "doomed".to_string())
            .await
            .unwrap()
            .is_some(),
        "an ambiguous request must not purge anything"
    );
}

/// Sending NEITHER `project` nor `project_id` is also a 400: the old CLI
/// always sent `project`, so a request missing both is a caller bug.
#[tokio::test]
async fn purge_project_refuses_neither_project_nor_project_id() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// The preview path (`dry_run: true`) must resolve `project_id` identically,
/// so `--project-id` works for the default no-confirm preview too.
#[tokio::test]
async fn purge_project_preview_accepts_project_id() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let (_ws, _keep, doomed) = seed_two_projects(&store, &state.wiki).await;

    let resp = post(
        state,
        "/admin/purge-project",
        json!({
            "workspace": "default",
            "project_id": doomed.to_string(),
            "confirm": false,
            "dry_run": true,
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the preview branch must resolve project_id too"
    );
}

// ---------------------------------------------------------------------------
// #1208 security boundary: a `project_id` is scoped to the named workspace
// ---------------------------------------------------------------------------
//
// `docs/security-boundaries.md` row 9 (scope resolution fail-closed): an
// operator holding an id must not be able to reach a row the workspace
// coordinate does not name. These are the adversarial cases plus the
// legitimate control, so a blanket deny cannot pass for a working guard.

/// Seed the ambiguity this feature exists for, at the real store level: a
/// project named `widget`, plus a second project carrying `widget` as its
/// legacy (v2 basename) key, so a NAME lookup for `widget` answers with two
/// holders and is refused. Returns `(the named row, the legacy twin)`.
async fn seed_legacy_twin(store: &Store) -> (WorkspaceId, ProjectId, ProjectId) {
    use ai_memory_core::repository_identity::{IdentitySource, IdentityStyle, RepositoryIdentity};

    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    // The row the NAME resolves to.
    let named = store
        .writer
        .get_or_create_project(ws, "widget", None)
        .await
        .unwrap();
    // A second row whose derived legacy key is also `widget`: captured from a
    // repository whose path-style name would be `widget`, so the store records
    // `widget` as its `legacy_name` while its current name differs.
    let twin = store
        .writer
        .resolve_project_by_identity_for_capture(
            ws,
            RepositoryIdentity {
                identity: "github.com/acme/widget".to_owned(),
                source: IdentitySource::GitRemote,
            },
            IdentityStyle::Path,
            "acme-widget",
            Some("/work/widget".to_owned()),
            None,
            None,
        )
        .await
        .unwrap()
        .0;
    assert_ne!(named, twin, "the twin must be a distinct row");
    (ws, named, twin)
}

/// The `project` name is not an address once two rows answer to it: the
/// lookup refuses instead of guessing, which is the pre-existing fail-closed
/// behaviour this feature makes escapable rather than removes. (#1208)
#[tokio::test]
async fn purge_project_still_refuses_an_ambiguous_name_by_default() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let (ws, named, twin) = seed_legacy_twin(&store).await;

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "widget", "confirm": true }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "an ambiguous name must stay refused, never silently resolved"
    );

    // Nothing was purged: both holders are still there.
    assert!(
        store
            .reader
            .find_project(ws, "widget".to_string())
            .await
            .unwrap()
            .is_some(),
        "the named row must survive a refused ambiguous purge"
    );
    assert!(
        store
            .reader
            .project_name_by_id(ws, twin)
            .await
            .unwrap()
            .is_some(),
        "the twin must survive a refused ambiguous purge"
    );
    assert!(named != twin);
}

/// The id reaches the twin the name could not, and it purges THAT row only --
/// the name-holder is left completely intact. This is the legitimate control
/// for the boundary: without it, a blanket deny would look like a passing
/// guard. (#1208)
#[tokio::test]
async fn purge_project_by_id_reaches_the_twin_and_leaves_the_name_holder_intact() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let (ws, named, twin) = seed_legacy_twin(&store).await;

    let resp = post(
        state,
        "/admin/purge-project",
        json!({
            "workspace": "default",
            "project_id": twin.to_string(),
            "confirm": true,
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the id must be able to name the row the ambiguous name could not"
    );

    assert!(
        store
            .reader
            .project_name_by_id(ws, twin)
            .await
            .unwrap()
            .is_none(),
        "the row named by project_id is the one purged"
    );
    assert!(
        store
            .reader
            .project_name_by_id(ws, named)
            .await
            .unwrap()
            .is_some(),
        "the sibling that owns the NAME must be untouched"
    );
    assert_eq!(
        store
            .reader
            .project_name_by_id(ws, named)
            .await
            .unwrap()
            .as_deref(),
        Some("widget"),
        "and it must still carry its own name"
    );
}

/// An id that really exists, but in ANOTHER workspace, is a 404 -- the same
/// answer as an id that does not exist. It must never resolve across the
/// workspace the request named. (#1208)
#[tokio::test]
async fn purge_project_refuses_a_real_id_from_another_workspace() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let other_ws = store
        .writer
        .get_or_create_workspace("elsewhere")
        .await
        .unwrap();
    let foreign = store
        .writer
        .get_or_create_project(other_ws, "secret", None)
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/purge-project",
        json!({
            "workspace": "default",
            "project_id": foreign.to_string(),
            "confirm": true,
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a real id from another workspace must not resolve here"
    );

    assert!(
        store
            .reader
            .project_name_by_id(other_ws, foreign)
            .await
            .unwrap()
            .is_some(),
        "the foreign row must be untouched"
    );
}
