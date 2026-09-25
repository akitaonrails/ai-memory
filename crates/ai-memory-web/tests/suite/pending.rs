//! Adversarial tests for the root-only `GET /pending` triage page.
//!
//! The page reads every project's pending auto-improvement queue, so it is a
//! cross-project entry point: it must refuse anyone `/admin` refuses. The
//! decisions themselves post to `/admin/pending-writes/*`; the session, CSRF,
//! and provenance tests for that path live in the MCP crate's
//! `admin_pending_writes_session` suite.

use std::sync::Arc;

use ai_memory_core::{ActorContext, AuthLevel, NewPage, NewUser, PagePath, Tier, UserRole};
use ai_memory_store::{
    AutoImproveProposalOperation, NewAutoImproveProposal, RejectAutoImproveProposal,
    StageAutoImproveRun, Store,
};
use ai_memory_web::{
    HtmlAuthRedirectConfig, WebMountSpec, html_auth_redirect_mw, router, split_web_routers,
};
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use tempfile::TempDir;
use tower::ServiceExt;

struct Fixture {
    _tmp: TempDir,
    store: Store,
    wiki: Wiki,
}

async fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    Fixture {
        _tmp: tmp,
        store,
        wiki,
    }
}

/// Stage one pending proposal and return its scope and id.
async fn stage(
    store: &Store,
    project: &str,
    path: &str,
    title: &str,
    operation: AutoImproveProposalOperation,
) -> (
    ai_memory_core::WorkspaceId,
    ai_memory_core::ProjectId,
    ai_memory_core::AutoImproveProposalId,
) {
    let (ws, proj, ids) = stage_in(store, "default", project, &[(path, title)], operation).await;
    (ws, proj, ids[0])
}

/// Stage one run with one proposal per `(path, title)`.
async fn stage_in(
    store: &Store,
    workspace: &str,
    project: &str,
    proposals: &[(&str, &str)],
    operation: AutoImproveProposalOperation,
) -> (
    ai_memory_core::WorkspaceId,
    ai_memory_core::ProjectId,
    Vec<ai_memory_core::AutoImproveProposalId>,
) {
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
    let staged = store
        .writer
        .stage_auto_improve_run(StageAutoImproveRun {
            workspace_id: ws,
            project_id: proj,
            session_id: None,
            provider: Some("test".into()),
            model: Some("model".into()),
            summary: Some("summary".into()),
            warnings_json: serde_json::json!([]),
            rejected_candidates_json: serde_json::json!([]),
            config_json: serde_json::json!({"mode": "stage"}),
            proposal_actor: ActorContext {
                agent: Some("auto_improve".into()),
                ..ActorContext::default()
            },
            proposals: proposals
                .iter()
                .map(|(path, title)| NewAutoImproveProposal {
                    operation,
                    target_path: PagePath::new(*path).unwrap(),
                    kind: "note".into(),
                    title: (*title).into(),
                    confidence: 0.9,
                    rationale: format!("rationale for {title}"),
                    evidence_json: serde_json::json!([{"source": "test"}]),
                    body_markdown: format!("# {title}\n\nproposed body"),
                    artifact_sha256: None,
                    edit_mode: None,
                    patch_json: None,
                    expected_base_body_sha256: None,
                })
                .collect(),
        })
        .await
        .unwrap();
    assert_eq!(
        staged.proposal_ids.len(),
        proposals.len(),
        "all proposals staged"
    );
    (ws, proj, staged.proposal_ids)
}

/// An `update` proposal only stages against a page that exists.
async fn seed_page(store: &Store, project: &str, path: &str) {
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, project, None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path).unwrap(),
            title: "Current".into(),
            body: "current body".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({"kind": "rule"}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
}

async fn add_db_user(store: &Store) {
    let new_user = NewUser {
        username: "alice".into(),
        name: None,
        email: None,
    };
    store
        .writer
        .create_human_user(new_user, UserRole::User, None, false)
        .await
        .unwrap();
}

/// The builtin browser behind a stub that stamps `level` the way
/// `require_dual_auth` does, wrapped in the real HTML auth redirect layer.
fn app(f: &Fixture, level: AuthLevel) -> axum::Router {
    let cfg = Arc::new(HtmlAuthRedirectConfig::from_mount("", "/"));
    router(f.store.reader.clone(), f.wiki.clone())
        .layer(axum::middleware::from_fn(
            move |mut req: Request<Body>, next: axum::middleware::Next| async move {
                req.extensions_mut().insert(level);
                next.run(req).await
            },
        ))
        .layer(axum::middleware::from_fn_with_state(
            cfg,
            html_auth_redirect_mw,
        ))
}

async fn get(app: axum::Router, uri: &str, html: bool) -> (StatusCode, Option<String>, String) {
    let mut req = Request::builder().uri(uri);
    if html {
        req = req
            .header(header::ACCEPT, "text/html")
            .header("sec-fetch-dest", "document");
    }
    let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let location = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, location, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn pending_page_refuses_anonymous_once_operators_are_distinguished() {
    let f = fixture().await;
    stage(
        &f.store,
        "alpha",
        "notes/a.md",
        "Alpha secret",
        AutoImproveProposalOperation::Create,
    )
    .await;
    add_db_user(&f.store).await;

    let (status, _, body) = get(app(&f, AuthLevel::Anonymous), "/pending", false).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!body.contains("Alpha secret"), "{body}");

    // A browser navigation lands on the login form, like the rest of `/web`.
    let (status, location, _) = get(app(&f, AuthLevel::Anonymous), "/pending", true).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        location
            .as_deref()
            .is_some_and(|l| l.starts_with("/login?next=")),
        "{location:?}"
    );
}

#[tokio::test]
async fn pending_page_refuses_a_db_user_without_leaking_or_redirecting() {
    let f = fixture().await;
    stage(
        &f.store,
        "alpha",
        "notes/a.md",
        "Alpha secret",
        AutoImproveProposalOperation::Create,
    )
    .await;
    add_db_user(&f.store).await;

    let (status, location, body) = get(app(&f, AuthLevel::User), "/pending", true).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Not "change your password": the 403 is the root-only refusal.
    assert_eq!(location, None);
    assert!(body.contains("Root access required"), "{body}");
    assert!(!body.contains("Alpha secret"), "{body}");
}

#[tokio::test]
async fn pending_page_lists_every_projects_queue_for_root() {
    let f = fixture().await;
    let (ws, proj, decided) = stage(
        &f.store,
        "alpha",
        "notes/decided.md",
        "Already rejected",
        AutoImproveProposalOperation::Create,
    )
    .await;
    f.store
        .writer
        .reject_auto_improve_proposal(RejectAutoImproveProposal {
            workspace_id: ws,
            project_id: proj,
            proposal_id: decided,
            reason: "no".into(),
            actor: ActorContext::anonymous(),
            author_id: None,
        })
        .await
        .unwrap();
    let (_, _, alpha) = stage(
        &f.store,
        "alpha",
        "notes/a.md",
        "Alpha <script>alert(1)</script>",
        AutoImproveProposalOperation::Create,
    )
    .await;
    seed_page(&f.store, "beta", "_rules/b.md").await;
    let (_, _, beta) = stage(
        &f.store,
        "beta",
        "_rules/b.md",
        "Beta rule",
        AutoImproveProposalOperation::Update,
    )
    .await;
    add_db_user(&f.store).await;

    let (status, _, body) = get(app(&f, AuthLevel::Root), "/pending", true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains(&format!("data-id=\"{alpha}\"")), "{body}");
    assert!(body.contains(&format!("data-id=\"{beta}\"")), "{body}");
    assert!(
        !body.contains(&decided.to_string()),
        "decided proposal listed"
    );
    assert!(body.contains("data-project=\"beta\""));
    assert!(body.contains("rationale for Beta rule"));
    // Proposal text is untrusted: it renders escaped, never as markup.
    assert!(!body.contains("<script>alert(1)</script>"), "{body}");
    assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    // `_rules/` and `update` proposals carry their warnings.
    assert!(body.contains("Every agent session loads those pages"));
    assert!(body.contains("replaces an existing page"));
    // Decisions go to the existing admin handlers, not to this crate.
    assert!(body.contains("\"/admin/pending-writes/\""));
}

#[tokio::test]
async fn pending_page_follows_the_admin_gate_on_a_single_operator_server() {
    // No users and no trusted proxy: `/admin` admits the anonymous
    // loopback operator, so this page does too.
    let f = fixture().await;
    stage(
        &f.store,
        "alpha",
        "notes/a.md",
        "Alpha",
        AutoImproveProposalOperation::Create,
    )
    .await;

    let (status, _, body) = get(app(&f, AuthLevel::Anonymous), "/pending", false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Alpha"));
}

#[tokio::test]
async fn pending_page_filters_by_project_and_ignores_unknown_values() {
    let f = fixture().await;
    let (_, _, alpha) = stage(
        &f.store,
        "alpha",
        "notes/a.md",
        "Alpha",
        AutoImproveProposalOperation::Create,
    )
    .await;
    let (_, _, beta) = stage(
        &f.store,
        "beta",
        "notes/b.md",
        "Beta",
        AutoImproveProposalOperation::Create,
    )
    .await;

    let (_, _, body) = get(
        app(&f, AuthLevel::Root),
        "/pending?project=default%2Fbeta&sort=confidence-asc",
        false,
    )
    .await;
    assert!(body.contains(&format!("data-id=\"{beta}\"")));
    assert!(!body.contains(&format!("data-id=\"{alpha}\"")));
    assert!(body.contains("<option value=\"default/beta\" selected>"));
    assert!(body.contains("<option value=\"confidence-asc\" selected>"));

    let (_, _, body) = get(
        app(&f, AuthLevel::Root),
        "/pending?project=default%2Fnope&sort=bogus",
        false,
    )
    .await;
    assert!(body.contains(&format!("data-id=\"{alpha}\"")));
    assert!(body.contains(&format!("data-id=\"{beta}\"")));
}

#[tokio::test]
async fn pending_page_is_read_only() {
    let f = fixture().await;
    let (_, _, id) = stage(
        &f.store,
        "alpha",
        "notes/a.md",
        "Alpha",
        AutoImproveProposalOperation::Create,
    )
    .await;

    for uri in [
        "/pending".to_owned(),
        format!("/pending/{id}/approve"),
        format!("/pending/{id}/reject"),
    ] {
        let resp = app(&f, AuthLevel::Root)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(&uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                resp.status(),
                StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_FOUND
            ),
            "{uri} accepted a POST: {}",
            resp.status()
        );
    }
}

#[tokio::test]
async fn pending_page_gate_sees_the_trusted_proxy_setting_through_the_mount() {
    // No `users` rows: only the trusted-proxy flag tells operators apart.
    // Dropping it on the way from `serve` to the page would admit every
    // proxy-asserted user as an admin.
    for (trusted_proxy_identity, expected) in
        [(true, StatusCode::FORBIDDEN), (false, StatusCode::OK)]
    {
        let f = fixture().await;
        stage(
            &f.store,
            "alpha",
            "notes/a.md",
            "Alpha",
            AutoImproveProposalOperation::Create,
        )
        .await;
        let web = split_web_routers(
            true,
            f.store.reader.clone(),
            f.wiki.clone(),
            WebMountSpec {
                web_ui_dir: None,
                cors_origins: &[],
                web_slug: "/web",
                base_href: "/web/",
                base_path: "",
                trusted_proxy_identity,
            },
        )
        .unwrap();
        let app = web.protected.layer(axum::middleware::from_fn(
            |mut req: Request<Body>, next: axum::middleware::Next| async move {
                req.extensions_mut().insert(AuthLevel::User);
                next.run(req).await
            },
        ));
        let (status, _, _) = get(app, "/web/pending", false).await;
        assert_eq!(
            status, expected,
            "trusted_proxy_identity = {trusted_proxy_identity}"
        );
    }
}

#[tokio::test]
async fn pending_page_filter_keys_do_not_collide_on_slashes_in_names() {
    // `a/b` + `c` and `a` + `b/c` print the same `workspace/project` label.
    let f = fixture().await;
    let (_, _, first) = stage_in(
        &f.store,
        "a/b",
        "c",
        &[("notes/one.md", "First")],
        AutoImproveProposalOperation::Create,
    )
    .await;
    let (_, _, second) = stage_in(
        &f.store,
        "a",
        "b/c",
        &[("notes/two.md", "Second")],
        AutoImproveProposalOperation::Create,
    )
    .await;

    let (_, _, body) = get(app(&f, AuthLevel::Root), "/pending", false).await;
    assert!(body.contains("<option value=\"a%2Fb/c\""), "{body}");
    assert!(body.contains("<option value=\"a/b%2Fc\""), "{body}");
    // Distinct on screen too, not only in the submitted value.
    assert!(body.contains("&quot;a/b&quot;/&quot;c&quot; (1)"), "{body}");
    assert!(body.contains("&quot;a&quot;/&quot;b/c&quot; (1)"), "{body}");

    let (_, _, body) = get(
        app(&f, AuthLevel::Root),
        "/pending?project=a%252Fb%2Fc",
        false,
    )
    .await;
    assert!(
        body.contains(&format!("data-id=\"{}\"", first[0])),
        "{body}"
    );
    assert!(
        !body.contains(&format!("data-id=\"{}\"", second[0])),
        "{body}"
    );
}

#[tokio::test]
async fn pending_page_counts_and_filters_projects_beyond_the_read_cap() {
    // 501 older proposals fill the read cap; the newer `late` project is
    // outside it, but it must still be counted and reachable.
    let f = fixture().await;
    let paths: Vec<String> = (0..501).map(|i| format!("notes/p{i}.md")).collect();
    let bulk: Vec<(&str, &str)> = paths.iter().map(|p| (p.as_str(), "Bulk")).collect();
    stage_in(
        &f.store,
        "default",
        "bulk",
        &bulk,
        AutoImproveProposalOperation::Create,
    )
    .await;
    let (_, _, late) = stage(
        &f.store,
        "late",
        "notes/late.md",
        "Late",
        AutoImproveProposalOperation::Create,
    )
    .await;

    let (_, _, body) = get(app(&f, AuthLevel::Root), "/pending", false).await;
    assert!(
        body.contains("All projects (502)"),
        "total must count every project"
    );
    assert!(body.contains("default/late (1)"));
    assert!(body.contains("only the oldest 500"));
    assert!(!body.contains(&format!("data-id=\"{late}\"")));

    let (_, _, body) = get(
        app(&f, AuthLevel::Root),
        "/pending?project=default%2Flate",
        false,
    )
    .await;
    assert!(
        body.contains(&format!("data-id=\"{late}\"")),
        "late project unreachable"
    );
    assert!(!body.contains("only the oldest 500"));
}
