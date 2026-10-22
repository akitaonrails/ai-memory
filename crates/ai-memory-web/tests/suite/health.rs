//! The root-only `GET /health` panel.
//!
//! It describes the whole server, so it must refuse anyone `/admin` refuses
//! (row 3b's pattern: anonymous 401 once operators are told apart, a database
//! user 403 without the change-password redirect, root and the single-operator
//! loopback admitted). Each check it renders is driven by a fixture here, and
//! the page is scanned for secret-shaped strings planted in a provider error,
//! a page body, a page title and a project name.

use std::sync::Arc;

use ai_memory_core::{
    ActorContext, AuthLevel, IngestMetrics, NewUser, PagePath, ProjectId, Tier, UserRole,
    WorkspaceId,
};
use ai_memory_llm::{
    ChatRequest, ChatResponse, Embedder, LlmError, LlmProvider, LlmResult, ProviderHealth,
    SyntheticEmbedder,
};
use ai_memory_store::Store;
use ai_memory_web::health::{HealthProbe, HealthSources, JournalState, ProcessState, SpoolState};
use ai_memory_web::{
    HtmlAuthRedirectConfig, Recall, WebMountSpec, html_auth_redirect_mw, split_web_routers,
};
use ai_memory_wiki::{Wiki, WritePageRequest};
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use tempfile::TempDir;
use tower::ServiceExt;

const PLANTED_PROVIDER: &str = "sk-PLANTEDPROVIDERSECRET0123456789";
const PLANTED_EMBED: &str = "sk-PLANTEDEMBEDDERSECRET0123456789";
const PLANTED_BODY: &str = "aim_PLANTEDBODYSECRET0123456789";
const PLANTED_TITLE: &str = "PLANTEDTITLESECRET";
const PLANTED_PROJECT: &str = "proj-PLANTEDSCOPESECRET";
const RETRY_HINT: &str = "ai-memory llm-test --provider openai --model gpt-test --prompt ping";

struct FailingLlm;

#[async_trait::async_trait]
impl LlmProvider for FailingLlm {
    fn name(&self) -> &'static str {
        "openai"
    }
    fn model(&self) -> &str {
        "gpt-test"
    }
    async fn complete(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Err(LlmError::Provider {
            status: 401,
            body: format!("invalid api key {PLANTED_PROVIDER}"),
            retry_after: None,
        })
    }
    async fn complete_structured_raw(
        &self,
        _request: ChatRequest,
        _schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        Err(LlmError::Provider {
            status: 401,
            body: PLANTED_PROVIDER.into(),
            retry_after: None,
        })
    }
}

struct FailingEmbedder;

#[async_trait::async_trait]
impl Embedder for FailingEmbedder {
    fn provider(&self) -> &'static str {
        "openai"
    }
    fn model(&self) -> &str {
        "text-embedding-test"
    }
    fn dim(&self) -> u32 {
        16
    }
    async fn embed(&self, _text: &str) -> LlmResult<Vec<f32>> {
        Err(LlmError::Provider {
            status: 503,
            body: format!("upstream said {PLANTED_EMBED}"),
            retry_after: None,
        })
    }
}

struct FixedProbe(ProcessState);

impl HealthProbe for FixedProbe {
    fn process_state(&self) -> ProcessState {
        self.0
    }
}

struct Fixture {
    _tmp: TempDir,
    store: Store,
    wiki: Wiki,
}

fn page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str) -> WritePageRequest {
    WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        frontmatter: serde_json::json!({"kind": "fact", "title": PLANTED_TITLE}),
        body: body.to_owned(),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ActorContext::anonymous(),
        evidence: Vec::new(),
    }
}

/// One page embedded under an 8-dim synthetic triple, one with no vector,
/// both carrying planted secrets in body and title, in a project whose name
/// is itself secret-shaped.
async fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, PLANTED_PROJECT, None)
        .await
        .unwrap();
    let embedding_wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_embedder(Arc::new(SyntheticEmbedder::new(8)));
    embedding_wiki
        .write_page(page(
            ws,
            proj,
            "notes/a.md",
            &format!("token {PLANTED_BODY}"),
        ))
        .await
        .unwrap();
    let wiki = embedding_wiki.without_embedder();
    wiki.write_page(page(
        ws,
        proj,
        "notes/b.md",
        &format!("other {PLANTED_BODY}"),
    ))
    .await
    .unwrap();
    Fixture {
        _tmp: tmp,
        store,
        wiki,
    }
}

/// Provider health with a failed LLM call and a failed embed recorded, the
/// way the wrappers record them in production.
async fn failing_providers() -> ProviderHealth {
    let health = ProviderHealth::default();
    let llm = health.wrap_llm_provider(
        Arc::new(FailingLlm),
        "openai",
        "gpt-test",
        Some(RETRY_HINT.into()),
    );
    let embedder = health.wrap_embedder(
        Arc::new(FailingEmbedder),
        "openai",
        "text-embedding-test",
        16,
    );
    assert!(
        llm.complete(ChatRequest::user_prompt("ping"))
            .await
            .is_err()
    );
    assert!(embedder.embed_query("ping").await.is_err());
    health
}

fn app(
    f: &Fixture,
    level: AuthLevel,
    health: Option<HealthSources>,
    recall: Recall,
) -> axum::Router {
    let cfg = Arc::new(HtmlAuthRedirectConfig::from_mount("", "/web"));
    split_web_routers(
        true,
        f.store.reader.clone(),
        f.wiki.clone(),
        WebMountSpec {
            enable_api: false,
            web_ui_dir: None,
            cors_origins: &[],
            web_slug: "/web",
            base_href: "/web/",
            base_path: "",
            trusted_proxy_identity: false,
            recall,
            health: health.map(Arc::new),
        },
    )
    .unwrap()
    .protected
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

async fn get(app: axum::Router, html: bool) -> (StatusCode, Option<String>, String) {
    let mut req = Request::builder().uri("/web/health");
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

async fn add_db_user(store: &Store) {
    store
        .writer
        .create_human_user(
            NewUser {
                username: "alice".into(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
}

/// Every process-side check failing at once: shed and failed hooks, a
/// near-full write queue, a full spool, an unreadable journal, both
/// providers erroring.
async fn failing_sources() -> HealthSources {
    let ingest = Arc::new(IngestMetrics::default());
    for _ in 0..3 {
        ingest.record_accepted();
    }
    ingest.record_shed_saturated();
    ingest.record_shed_saturated();
    ingest.record_shed_rate_limited();
    ingest.record_failed();
    HealthSources {
        ingest,
        providers: failing_providers().await,
        probe: Arc::new(FixedProbe(ProcessState {
            write_queue: Some((90, 100)),
            spool: Some(SpoolState {
                pending: 500,
                oldest_age_ms: Some(7_200_000),
                retries_total: 12,
                capacity: 500,
            }),
            journal: Some(JournalState {
                unreadable: true,
                ..JournalState::default()
            }),
        })),
    }
}

fn configured_embedder() -> Recall {
    // 16 dims against vectors stored at 8: the stored ones are stale.
    Recall {
        embedder: Some(Arc::new(SyntheticEmbedder::new(16))),
        rerank: None,
    }
}

fn assert_check(body: &str, id: &str, fix: Option<&str>) {
    let start = body
        .find(&format!("data-check=\"{id}\""))
        .unwrap_or_else(|| panic!("check {id} not rendered:\n{body}"));
    let item = &body[start..];
    let item = &item[..item.find("</li>").unwrap_or(item.len())];
    match fix {
        Some(fix) => assert!(
            item.contains(&format!("Fix: <code class=\"font-mono\">{fix}</code>")),
            "check {id} lacks its fix line {fix:?}:\n{item}"
        ),
        None => assert!(!item.contains("Fix:"), "check {id} has a fix line:\n{item}"),
    }
}

#[tokio::test]
async fn health_page_renders_each_failing_check_with_its_fix_line() {
    let f = fixture().await;
    let (status, _, body) = get(
        app(
            &f,
            AuthLevel::Root,
            Some(failing_sources().await),
            configured_embedder(),
        ),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_check(&body, "hook_shed", None);
    assert_check(&body, "hook_failed", None);
    assert_check(&body, "write_queue", None);
    assert_check(&body, "spool_evicting", Some("ai-memory recover"));
    assert_check(&body, "journal_unreadable", Some("ai-memory recover"));
    assert_check(&body, "llm_provider", Some(RETRY_HINT));
    assert_check(&body, "embedding_provider", None);
    assert_check(&body, "stale_vectors", Some("ai-memory embed --force"));
    assert_check(&body, "missing_vectors", Some("ai-memory embed"));
    assert!(body.contains("HTTP 401"), "{body}");
    // The counters behind them.
    assert!(body.contains("3 (2 saturated, 1 rate-limited)"), "{body}");
    assert!(body.contains("90/100"), "{body}");
    assert!(
        body.contains("500 pending of 500 (oldest 2h, 12 retries)"),
        "{body}"
    );
    assert!(
        body.contains("synthetic/bag-of-words-v1 (dim 16)"),
        "{body}"
    );
    assert!(
        body.contains("synthetic/bag-of-words-v1 dim 8: 1 rows"),
        "{body}"
    );
    assert!(
        body.contains("0 pending, 0 running, 0 completed, 0 failed"),
        "{body}"
    );
    assert!(!body.contains("data-check=\"none\""), "{body}");
}

/// A page whose only vector was made under another triple must fire
/// `missing_vectors` now that the count is computed against the configured
/// embedder's triple: before the count knew the triple, such a page read
/// as covered and the check stayed silent.
#[tokio::test]
async fn missing_vectors_fires_when_the_only_vector_is_from_another_triple() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "models", None)
        .await
        .unwrap();
    // A single page; its only vector is the old model's (8-dim synthetic)
    // triple.
    let embedding_wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_embedder(Arc::new(SyntheticEmbedder::new(8)));
    embedding_wiki
        .write_page(page(
            ws,
            proj,
            "notes/old-model.md",
            "the only vector here is stale",
        ))
        .await
        .unwrap();
    let wiki = embedding_wiki.without_embedder();
    let f = Fixture {
        _tmp: tmp,
        store,
        wiki,
    };
    let sources = HealthSources {
        ingest: Arc::new(IngestMetrics::default()),
        providers: ProviderHealth::default(),
        probe: Arc::new(FixedProbe(ProcessState {
            write_queue: Some((0, 100)),
            spool: Some(SpoolState {
                capacity: 500,
                ..SpoolState::default()
            }),
            journal: Some(JournalState::default()),
        })),
    };
    // The configured embedder is the new model (16-dim synthetic).
    let (status, _, body) = get(
        app(&f, AuthLevel::Root, Some(sources), configured_embedder()),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_check(&body, "missing_vectors", Some("ai-memory embed"));
    // The counter behind it: one latest page without a 16-dim vector.
    assert!(body.contains("1 latest pages missing"), "{body}");
    // The row still shows up as a stored triple — the page is not
    // silently dropped from the index picture.
    assert!(
        body.contains("synthetic/bag-of-words-v1 dim 8: 1 rows"),
        "{body}"
    );
}

#[tokio::test]
async fn health_page_renders_pending_spool_and_journal_checks() {
    let f = fixture().await;
    let sources = HealthSources {
        ingest: Arc::new(IngestMetrics::default()),
        providers: ProviderHealth::default(),
        probe: Arc::new(FixedProbe(ProcessState {
            write_queue: Some((0, 100)),
            spool: Some(SpoolState {
                pending: 4,
                oldest_age_ms: Some(30_000),
                retries_total: 1,
                capacity: 500,
            }),
            journal: Some(JournalState {
                pending: 2,
                degraded: 1,
                finish_failed: 1,
                unreadable: false,
            }),
        })),
    };
    let (status, _, body) = get(
        app(&f, AuthLevel::Root, Some(sources), Recall::default()),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_check(&body, "spool_pending", Some("ai-memory recover"));
    assert_check(&body, "journal_pending", Some("ai-memory recover"));
    // No embedder configured: vector checks stay quiet.
    assert!(!body.contains("data-check=\"stale_vectors\""), "{body}");
    assert!(!body.contains("data-check=\"missing_vectors\""), "{body}");
    assert!(!body.contains("data-check=\"write_queue\""), "{body}");
}

#[tokio::test]
async fn health_page_reports_a_healthy_server_as_passing() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let f = Fixture {
        _tmp: tmp,
        store,
        wiki,
    };
    let sources = HealthSources {
        ingest: Arc::new(IngestMetrics::default()),
        providers: ProviderHealth::default(),
        probe: Arc::new(FixedProbe(ProcessState {
            write_queue: Some((0, 100)),
            spool: Some(SpoolState {
                capacity: 500,
                ..SpoolState::default()
            }),
            journal: Some(JournalState::default()),
        })),
    };
    let (status, _, body) = get(
        app(&f, AuthLevel::Root, Some(sources), Recall::default()),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("data-check=\"none\""), "{body}");
    assert!(!body.contains("This server did not wire"), "{body}");

    // A host that wired nothing says so instead of showing zeros as fact.
    let (_, _, body) = get(app(&f, AuthLevel::Root, None, Recall::default()), true).await;
    assert!(body.contains("This server did not wire"), "{body}");
}

#[tokio::test]
async fn health_page_carries_no_secret_material() {
    let f = fixture().await;
    let (status, _, body) = get(
        app(
            &f,
            AuthLevel::Root,
            Some(failing_sources().await),
            configured_embedder(),
        ),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for planted in [
        PLANTED_PROVIDER,
        PLANTED_EMBED,
        PLANTED_BODY,
        PLANTED_TITLE,
        PLANTED_PROJECT,
        "PLANTED",
        "invalid api key",
        "upstream said",
    ] {
        assert!(
            !body.contains(planted),
            "health page leaked {planted}:\n{body}"
        );
    }
}

#[tokio::test]
async fn health_page_refuses_anonymous_once_operators_are_distinguished() {
    let f = fixture().await;
    add_db_user(&f.store).await;
    let sources = failing_sources().await;
    let (status, _, body) = get(
        app(
            &f,
            AuthLevel::Anonymous,
            Some(sources.clone()),
            Recall::default(),
        ),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!body.contains("Server health"), "{body}");
    assert!(!body.contains("ai-memory recover"), "{body}");

    let (status, location, _) = get(
        app(&f, AuthLevel::Anonymous, Some(sources), Recall::default()),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        location
            .as_deref()
            .is_some_and(|l| l.starts_with("/web/login?next=")),
        "{location:?}"
    );
}

#[tokio::test]
async fn health_page_refuses_a_db_user_and_admits_root() {
    let f = fixture().await;
    add_db_user(&f.store).await;
    let sources = failing_sources().await;
    let (status, location, body) = get(
        app(
            &f,
            AuthLevel::User,
            Some(sources.clone()),
            Recall::default(),
        ),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(location, None, "a non-root 403 is not a password redirect");
    assert!(body.contains("Root access required"), "{body}");
    for leaked in ["Server health", "ai-memory recover", "90/100", "saturated"] {
        assert!(!body.contains(leaked), "DB user saw {leaked}: {body}");
    }

    // Control: root on the same multi-user server.
    let (status, _, body) = get(
        app(&f, AuthLevel::Root, Some(sources), Recall::default()),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Server health"), "{body}");
}

#[tokio::test]
async fn health_page_follows_the_admin_gate_on_a_single_operator_server() {
    // No users and no trusted proxy: `/admin` admits the anonymous loopback
    // operator, so this page does too.
    let f = fixture().await;
    let (status, _, body) = get(
        app(
            &f,
            AuthLevel::Anonymous,
            Some(failing_sources().await),
            Recall::default(),
        ),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Server health"), "{body}");
}

#[tokio::test]
async fn health_page_has_no_write_route() {
    let f = fixture().await;
    for method in [Method::POST, Method::PUT, Method::DELETE] {
        let resp = app(
            &f,
            AuthLevel::Root,
            Some(failing_sources().await),
            Recall::default(),
        )
        .oneshot(
            Request::builder()
                .method(method.clone())
                .uri("/web/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED, "{method}");
    }
}
