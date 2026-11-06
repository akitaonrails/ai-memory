//! Approving an auto-improve proposal through a `reject`-policy admission
//! chain: which failures land in `auto_improve_rejections`.

use std::time::Duration;

use ai_memory_core::{ActorContext, PagePath, ProjectId, WorkspaceId};
use ai_memory_store::{
    AutoImproveProposalOperation, AutoImproveProposalStatus, NewAutoImproveProposal,
    StageAutoImproveRun, Store,
};
use ai_memory_wiki::{AdmissionChain, AdmissionOp, FailurePolicy, WebhookConfig, Wiki};
use axum::Router;
use axum::http::StatusCode;
use axum::routing::post;
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    store: Store,
    wiki: Wiki,
    ws: WorkspaceId,
    proj: ProjectId,
}

async fn fixture(chain: AdmissionChain) -> Fixture {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "a", None)
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone())
        .with_admission_chain(chain);
    std::fs::create_dir_all(wiki.project_root(ws, proj)).unwrap();
    Fixture {
        _tmp: tmp,
        store,
        wiki,
        ws,
        proj,
    }
}

async fn spawn_server(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn chain(url: String, timeout_ms: u64, failure_policy: FailurePolicy) -> AdmissionChain {
    AdmissionChain::new(vec![WebhookConfig {
        name: "policy".into(),
        url,
        timeout_ms,
        failure_policy,
        events: vec![AdmissionOp::WritePage],
        blocking: true,
        projects: Vec::new(),
        headers: Vec::new(),
    }])
    .unwrap()
}

async fn stage_one(fx: &Fixture) -> ai_memory_core::AutoImproveProposalId {
    fx.store
        .writer
        .stage_auto_improve_run(StageAutoImproveRun {
            workspace_id: fx.ws,
            project_id: fx.proj,
            session_id: None,
            provider: None,
            model: None,
            summary: None,
            warnings_json: serde_json::json!([]),
            rejected_candidates_json: serde_json::json!([]),
            config_json: serde_json::json!({}),
            proposal_actor: ActorContext::anonymous(),
            proposals: vec![NewAutoImproveProposal {
                operation: AutoImproveProposalOperation::Create,
                target_path: PagePath::new("decisions/rule.md").unwrap(),
                kind: "rule".into(),
                title: "Rule".into(),
                confidence: 0.9,
                rationale: "because".into(),
                evidence_json: serde_json::json!([]),
                body_markdown: "# Rule\n\nAlways do the thing.\n".into(),
                artifact_sha256: None,
                edit_mode: None,
                patch_json: None,
                expected_base_body_sha256: None,
            }],
        })
        .await
        .unwrap()
        .proposal_ids[0]
}

/// Approve once and return (approval errored, proposal status, rejection reasons).
async fn approve(fx: &Fixture) -> (bool, AutoImproveProposalStatus, Vec<String>) {
    let id = stage_one(fx).await;
    let res = fx
        .wiki
        .approve_auto_improve_proposal(fx.ws, fx.proj, id, ActorContext::anonymous(), None, None)
        .await;
    let status = fx
        .store
        .reader
        .auto_improve_proposal_detail(fx.ws, fx.proj, id)
        .await
        .unwrap()
        .unwrap()
        .summary
        .status;
    let reasons = fx
        .store
        .reader
        .recent_auto_improve_rejections(fx.ws, fx.proj, 10, None)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.reason)
        .collect();
    (res.is_err(), status, reasons)
}

// Negative: an admission server that times out did not reject anything.
#[tokio::test]
async fn reject_policy_timeout_is_not_recorded_as_a_rejection() {
    let app = Router::new().route(
        "/policy",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            StatusCode::OK
        }),
    );
    let base = spawn_server(app).await;
    let fx = fixture(chain(format!("{base}/policy"), 200, FailurePolicy::Reject)).await;
    let (errored, status, reasons) = approve(&fx).await;
    assert!(errored);
    assert_eq!(status, AutoImproveProposalStatus::Failed);
    assert!(
        reasons.is_empty(),
        "outage recorded as rejection: {reasons:?}"
    );
}

// Negative: a 503 from the admission server is an outage, not a verdict.
#[tokio::test]
async fn reject_policy_503_is_not_recorded_as_a_rejection() {
    let app = Router::new().route(
        "/policy",
        post(|| async { (StatusCode::SERVICE_UNAVAILABLE, "down for maintenance") }),
    );
    let base = spawn_server(app).await;
    let fx = fixture(chain(
        format!("{base}/policy"),
        1_000,
        FailurePolicy::Reject,
    ))
    .await;
    let (errored, status, reasons) = approve(&fx).await;
    assert!(errored);
    assert_eq!(status, AutoImproveProposalStatus::Failed);
    assert!(
        reasons.is_empty(),
        "outage recorded as rejection: {reasons:?}"
    );
}

// Negative: 408 and 429 are transient availability responses, not policy verdicts.
#[tokio::test]
async fn reject_policy_transient_client_status_is_not_recorded_as_a_rejection() {
    for status_code in [StatusCode::REQUEST_TIMEOUT, StatusCode::TOO_MANY_REQUESTS] {
        let app = Router::new().route(
            "/policy",
            post(move || async move { (status_code, "try again later") }),
        );
        let base = spawn_server(app).await;
        let fx = fixture(chain(
            format!("{base}/policy"),
            1_000,
            FailurePolicy::Reject,
        ))
        .await;
        let (errored, status, reasons) = approve(&fx).await;
        assert!(errored);
        assert_eq!(status, AutoImproveProposalStatus::Failed);
        assert!(
            reasons.is_empty(),
            "{status_code} outage recorded as rejection: {reasons:?}"
        );
    }
}

// Control: an explicit 403 is a policy verdict and must stay a rejection.
#[tokio::test]
async fn reject_policy_403_is_recorded_as_a_rejection() {
    let app = Router::new().route(
        "/policy",
        post(|| async { (StatusCode::FORBIDDEN, "no secrets") }),
    );
    let base = spawn_server(app).await;
    let fx = fixture(chain(
        format!("{base}/policy"),
        1_000,
        FailurePolicy::Reject,
    ))
    .await;
    let (errored, status, reasons) = approve(&fx).await;
    assert!(errored);
    assert_eq!(status, AutoImproveProposalStatus::Failed);
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(reasons[0].contains("403"), "{reasons:?}");
}

// Control: with the ignore policy an outage neither fails nor rejects.
#[tokio::test]
async fn ignore_policy_timeout_approves_without_rejection() {
    let app = Router::new().route(
        "/policy",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            StatusCode::OK
        }),
    );
    let base = spawn_server(app).await;
    let fx = fixture(chain(format!("{base}/policy"), 200, FailurePolicy::Ignore)).await;
    let (errored, status, reasons) = approve(&fx).await;
    assert!(!errored);
    assert_eq!(status, AutoImproveProposalStatus::Approved);
    assert!(reasons.is_empty(), "{reasons:?}");
}
