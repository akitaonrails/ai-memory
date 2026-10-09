//! Integration tests for `POST /admin/write-page`.
//!
//! Exercises the route through the axum router: post a synthetic page,
//! verify it appears in `/admin/search` results. Also tests that an
//! unknown tier returns 422.

use ai_memory_core::repository_identity::{IdentitySource, IdentityStyle, RepositoryIdentity};
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;

async fn make_state(tmp: &TempDir) -> AdminState {
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let db_path = store.db_path().to_path_buf();
    AdminState {
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
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
        db_path,
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    }
}

async fn post_json(
    state: AdminState,
    uri: &str,
    body: serde_json::Value,
) -> axum::response::Response {
    let router = admin_router(state);
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    router.oneshot(req).await.unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn seed_identity_project(
    state: &AdminState,
) -> (ai_memory_core::WorkspaceId, ai_memory_core::ProjectId) {
    let workspace = state
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let (project, _) = state
        .writer
        .resolve_project_by_identity(
            workspace,
            RepositoryIdentity {
                identity: "github.com/acme/api".into(),
                source: IdentitySource::GitRemote,
            },
            IdentityStyle::HostPath,
            "api",
            None,
            None,
            None,
        )
        .await
        .unwrap();
    (workspace, project)
}

#[tokio::test]
async fn write_page_returns_page_id_and_path() {
    let tmp = TempDir::new().unwrap();
    let state = make_state(&tmp).await;

    let resp = post_json(
        state,
        "/admin/write-page",
        json!({
            "workspace": "default",
            "project": "scratch",
            "path": "notes/test-write.md",
            "body": "This is a test page written via the admin route.",
            "tier": "semantic",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "write-page must succeed");

    let body = body_json(resp).await;
    assert!(
        body["page_id"].is_string(),
        "response must have page_id: {body}"
    );
    assert_eq!(
        body["path"].as_str().unwrap(),
        "notes/test-write.md",
        "response path must match request: {body}"
    );
}

#[tokio::test]
async fn write_page_surfaces_manifest_failure_after_promotion() {
    let tmp = TempDir::new().unwrap();
    let state = make_state(&tmp).await;
    let (workspace, project) = seed_identity_project(&state).await;
    state.wiki.backfill_scope_manifests().await.unwrap();
    let manifest = tmp
        .path()
        .join("wiki")
        .join(workspace.to_string())
        .join(project.to_string())
        .join("_meta.md");
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir(&manifest).unwrap();

    let resp = post_json(
        state,
        "/admin/write-page",
        json!({
            "workspace": "default",
            "project": "acme-api",
            "path": "notes/promoted.md",
            "body": "survives the manifest failure",
            "tier": "semantic",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(
        body["manifest_warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("committed")),
        "{body}"
    );
}

#[tokio::test]
async fn write_page_appears_in_search() {
    let tmp = TempDir::new().unwrap();
    let state = make_state(&tmp).await;

    // Write a page with a distinctive term.
    let write_resp = post_json(
        state.clone(),
        "/admin/write-page",
        json!({
            "workspace": "default",
            "project": "scratch",
            "path": "notes/unique-term.md",
            "body": "The xyloquartz pattern enables distributed widget fusion.",
            "tier": "semantic",
        }),
    )
    .await;
    assert_eq!(write_resp.status(), StatusCode::OK);

    // Now search for the distinctive term.
    let router = admin_router(state);
    let search_req = Request::builder()
        .method("GET")
        .uri("/admin/search?q=xyloquartz&limit=10")
        .body(Body::empty())
        .unwrap();
    let search_resp = router.oneshot(search_req).await.unwrap();
    assert_eq!(search_resp.status(), StatusCode::OK);

    let hits: Vec<serde_json::Value> = serde_json::from_slice(
        &axum::body::to_bytes(search_resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "written page must appear in search results: {hits:?}"
    );
    assert_eq!(hits[0]["path"].as_str().unwrap(), "notes/unique-term.md");
}

#[tokio::test]
async fn write_page_invalid_tier_returns_422() {
    let tmp = TempDir::new().unwrap();
    let state = make_state(&tmp).await;

    let resp = post_json(
        state,
        "/admin/write-page",
        json!({
            "workspace": "default",
            "project": "scratch",
            "path": "notes/bad-tier.md",
            "body": "Some content.",
            "tier": "legendary",
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown tier must return 422"
    );

    let body = body_json(resp).await;
    assert!(
        body["error"].as_str().unwrap_or("").contains("legendary"),
        "error must mention the unknown tier name: {body}"
    );
}

#[tokio::test]
async fn write_page_two_projects_same_path_no_collision() {
    // Two projects can hold pages with the same `pages.path` without
    // colliding on disk — the per-project UUID-keyed layout (CLAUDE.md
    // §15) guarantees structural isolation. The wiki crate already
    // exercises this at the `Wiki::write_page` level; this test makes
    // sure the invariant survives through the full
    // `POST /admin/write-page` handler path.
    let tmp = TempDir::new().unwrap();
    let state = make_state(&tmp).await;

    // Body 1: alpha project.
    let resp = post_json(
        state.clone(),
        "/admin/write-page",
        json!({
            "workspace": "default",
            "project": "alpha",
            "path": "decisions/0001.md",
            "body": "Page from project alpha — fingerprint AAAA-alpha.",
            "tier": "semantic",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let alpha_id = body_json(resp).await["page_id"]
        .as_str()
        .expect("alpha page_id")
        .to_string();

    // Body 2: beta project, SAME page path.
    let resp = post_json(
        state.clone(),
        "/admin/write-page",
        json!({
            "workspace": "default",
            "project": "beta",
            "path": "decisions/0001.md",
            "body": "Page from project beta — fingerprint BBBB-beta.",
            "tier": "semantic",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let beta_id = body_json(resp).await["page_id"]
        .as_str()
        .expect("beta page_id")
        .to_string();

    // Distinct page rows.
    assert_ne!(
        alpha_id, beta_id,
        "the two writes must produce distinct page_ids"
    );

    // FTS5 search: both fingerprints findable, exactly one hit each.
    let resp = post_json(state.clone(), "/admin/search?q=AAAA-alpha", json!(null)).await;
    // /admin/search is a GET, not POST — re-route via the router.
    drop(resp);
    let router = admin_router(state.clone());
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/search?q=fingerprint")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let hits: serde_json::Value = body_json(resp).await;
    let hits_arr = hits.as_array().expect("array");
    assert_eq!(
        hits_arr.len(),
        2,
        "both projects' pages must be searchable: {hits}"
    );
    // Same wiki path appears on both hits.
    assert!(
        hits_arr
            .iter()
            .all(|h| h["path"].as_str() == Some("decisions/0001.md")),
        "both hits should share the same relative path: {hits}"
    );

    // Files on disk: both exist at their per-project namespaced paths.
    // The two project_id UUIDs differ, so two distinct files live on
    // disk even though pages.path is identical. Recursive walk via
    // std::fs (no walkdir dep) — collect every `decisions/0001.md`
    // we find under wiki/.
    let wiki_dir = tmp.path().join("wiki");
    let mut on_disk: Vec<std::path::PathBuf> = Vec::new();
    collect_files_named(&wiki_dir, "decisions/0001.md", &mut on_disk);
    assert_eq!(
        on_disk.len(),
        2,
        "expected two physical files for the same page path; found: {on_disk:?}"
    );
}

/// Recurse into `dir`, collecting every file whose relative path
/// (from `dir`) ends with `suffix`. Test helper kept inline because
/// it's only used here and we don't want a walkdir dep.
fn collect_files_named(dir: &std::path::Path, suffix: &str, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_dir() {
            collect_files_named(&p, suffix, out);
        } else if ft.is_file() && p.to_string_lossy().replace('\\', "/").ends_with(suffix) {
            out.push(p);
        }
    }
}

#[tokio::test]
async fn write_page_with_tags_and_pinned() {
    let tmp = TempDir::new().unwrap();
    let state = make_state(&tmp).await;

    let resp = post_json(
        state,
        "/admin/write-page",
        json!({
            "workspace": "default",
            "project": "scratch",
            "path": "notes/tagged.md",
            "body": "Tagged and pinned content.",
            "tier": "procedural",
            "tags": ["rust", "memory"],
            "pinned": true,
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "tagged+pinned page must succeed"
    );

    let body = body_json(resp).await;
    assert!(body["page_id"].is_string(), "must have page_id: {body}");
}

#[tokio::test]
async fn write_page_metadata_roundtrip_and_total_replacement() {
    let tmp = TempDir::new().unwrap();
    let state = make_state(&tmp).await;
    let ws = state
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = state
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let path = ai_memory_core::PagePath::new("notes/metadata.md").unwrap();
    assert_eq!(post_json(state.clone(), "/admin/write-page", json!({
        "workspace": "default", "project": "scratch", "path": "gotchas/build.md", "body": "Build problem."
    })).await.status(), StatusCode::OK);
    let mut request = json!({
        "workspace": "default", "project": "scratch", "path": path.as_str(),
        "body": "# Metadata\n\nFirst version.", "kind": " rule ",
        "entities": ["SQLite", " sqlite ", "Writer\nActor"],
        "abstract": " One-line summary. ",
        "relations": {"fixes": ["gotchas/build"], "causes": ["other:notes/problem.md"], "contradicts": ["decisions/old.md"]}
    });
    let resp = post_json(state.clone(), "/admin/write-page", request.clone()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let md = state.wiki.read_page(ws, proj, &path).unwrap();
    assert_eq!(md.frontmatter["kind"], "rule");
    assert_eq!(
        md.frontmatter["entities"],
        json!(["sqlite", "writer actor"])
    );
    assert_eq!(md.frontmatter["abstract"], "One-line summary.");
    assert_eq!(md.frontmatter["relations"], request["relations"]);
    let links = state
        .reader
        .page_links(ws, proj, path.as_str().into(), None)
        .await
        .unwrap();
    assert_eq!(links.links.len(), 1);
    assert_eq!(links.links[0].path, "gotchas/build.md");

    for key in ["kind", "entities", "abstract", "relations"] {
        request.as_object_mut().unwrap().remove(key);
    }
    assert_eq!(
        post_json(state.clone(), "/admin/write-page", request)
            .await
            .status(),
        StatusCode::OK
    );
    let md = state.wiki.read_page(ws, proj, &path).unwrap();
    for key in ["kind", "entities", "abstract", "relations"] {
        assert!(
            md.frontmatter.get(key).is_none(),
            "{key} must not survive replacement"
        );
    }
    assert!(
        state
            .reader
            .page_links(ws, proj, path.as_str().into(), None)
            .await
            .unwrap()
            .links
            .is_empty()
    );
}

#[tokio::test]
async fn write_page_metadata_admin_legacy_kind_matches_base() {
    let tmp = TempDir::new().unwrap();
    let state = make_state(&tmp).await;
    let ws = state
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = state
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    // The base adapter only trimmed kind and omitted an empty result. In
    // particular, it imposed neither a length nor a control-character check.
    for (i, (kind, expected)) in [
        (Some("x".repeat(65)), Some("x".repeat(65))),
        // Wiki's existing sanitizer removes NUL after the legacy adapter.
        (Some(" custom\0kind ".into()), Some("customkind".into())),
        (Some("  ".into()), None),
        (None, None),
    ]
    .into_iter()
    .enumerate()
    {
        let path = ai_memory_core::PagePath::new(format!("notes/legacy-kind-{i}.md")).unwrap();
        let resp = post_json(
            state.clone(),
            "/admin/write-page",
            json!({
                "workspace": "default", "project": "scratch", "path": path.as_str(),
                "body": "# Legacy kind\n\nUnchanged caller.", "kind": kind,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "base accepted {kind:?}");
        let md = state.wiki.read_page(ws, proj, &path).unwrap();
        assert_eq!(
            md.frontmatter
                .get("kind")
                .and_then(serde_json::Value::as_str),
            expected.as_deref()
        );
    }
}

#[tokio::test]
async fn write_page_metadata_rejects_invalid_payload_before_scope_creation() {
    let tmp = TempDir::new().unwrap();
    let state = make_state(&tmp).await;
    for metadata in [
        json!({"entities": "sqlite"}),
        json!({"entities": [42]}),
        json!({"entities": ["x".repeat(65)]}),
        json!({"entities": [format!("{}      ", "x".repeat(60))]}),
        json!({"entities": vec!["entity"; 11]}),
        json!({"abstract": "x".repeat(1025)}),
        json!({"relations": {"supports": ["notes/x.md"]}}),
        json!({"relations": {"fixes": ["../outside.md"]}}),
        json!({"relations": {"fixes": ["other :notes/x"]}}),
        json!({"relations": {"fixes": vec!["notes/x.md"; 33]}}),
    ] {
        let mut request = json!({"workspace": "invalid", "project": "invalid", "path": "notes/x.md", "body": "Refused."});
        request
            .as_object_mut()
            .unwrap()
            .extend(metadata.as_object().unwrap().clone());
        let resp = post_json(state.clone(), "/admin/write-page", request).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{metadata}"
        );
        assert!(
            state
                .reader
                .find_workspace("invalid".into())
                .await
                .unwrap()
                .is_none()
        );
    }
    // A legitimate request must still pass through the same route.
    assert_eq!(
        post_json(
            state,
            "/admin/write-page",
            json!({
                "workspace": "default", "project": "scratch", "path": "notes/x.md",
                "body": "Accepted.", "kind": "fact", "entities": ["sqlite"]
            })
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn write_page_metadata_preserves_admin_auth_and_sanitization() {
    use ai_memory_core::{ActorContext, AuthLevel, NewUser, PagePath, UserRole};
    let tmp = TempDir::new().unwrap();
    let state = make_state(&tmp).await;
    let ws = state
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = state
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let user = state
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
    let path = PagePath::new("notes/guard.md").unwrap();
    let payload = json!({
        "workspace": "default", "project": "scratch", "path": path.as_str(),
        "body": "# Guard\n\nLegitimate content.", "kind": "fact", "entities": ["sqlite"],
        "abstract": "token sk-1234567890abcdef", "relations": {"fixes": ["notes/target"]},
        "author_id": user.to_string(), "last_modified_by": {"username": "alice"},
        "frontmatter": {"workspace_id": "foreign", "author_id": user.to_string()}
    });
    for (level, expected) in [
        (AuthLevel::Anonymous, StatusCode::UNAUTHORIZED),
        (AuthLevel::User, StatusCode::FORBIDDEN),
        (AuthLevel::Root, StatusCode::OK),
    ] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/admin/write-page")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&payload).unwrap()))
            .unwrap();
        request.extensions_mut().insert(level);
        request.extensions_mut().insert(ActorContext {
            user: Some("root".into()),
            ..Default::default()
        });
        let resp = admin_router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(resp.status(), expected);
        assert_eq!(
            state.wiki.abs_path(ws, proj, &path).exists(),
            level == AuthLevel::Root
        );
    }
    let md = state.wiki.read_page(ws, proj, &path).unwrap();
    assert_eq!(md.frontmatter["last_modified_by"]["username"], "root");
    assert!(
        !md.frontmatter["abstract"]
            .as_str()
            .unwrap()
            .contains("sk-1234567890abcdef")
    );
    for key in ["workspace_id", "project_id", "author_id", "frontmatter"] {
        assert!(md.frontmatter.get(key).is_none(), "{key}");
    }
    let meta = state
        .reader
        .page_meta("default", "scratch", path.as_str())
        .await
        .unwrap()
        .unwrap();
    assert!(
        meta.author.is_none(),
        "root must not inherit the forged DB-user author"
    );
}

#[tokio::test]
async fn write_page_metadata_store_failure_rolls_back_and_recovers() {
    let tmp = TempDir::new().unwrap();
    let state = make_state(&tmp).await;
    let ws = state
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = state
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let path = ai_memory_core::PagePath::new("notes/recovery.md").unwrap();
    let mut request = json!({
        "workspace": "default", "project": "scratch", "path": path.as_str(),
        "body": "# Recovery\n\nOriginal body.", "abstract": "Original summary.",
        "entities": ["sqlite"], "relations": {"fixes": ["notes/target"]}
    });
    assert_eq!(
        post_json(state.clone(), "/admin/write-page", request.clone())
            .await
            .status(),
        StatusCode::OK
    );
    let original = state.wiki.read_page(ws, proj, &path).unwrap();
    let original_id = state
        .reader
        .latest_page_id_by_ids(ws, proj, path.as_str().into())
        .await
        .unwrap();
    // Fail the SQL insert after Wiki has installed the replacement on disk.
    let conn = rusqlite::Connection::open(&state.db_path).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_metadata BEFORE INSERT ON pages
        WHEN json_extract(NEW.frontmatter_json, '$.abstract') = 'Refused summary.'
        BEGIN SELECT RAISE(ABORT, 'injected metadata store failure'); END;",
    )
    .unwrap();
    request["abstract"] = json!("Refused summary.");
    request["body"] = json!("# Recovery\n\nReplacement body.");
    assert_eq!(
        post_json(state.clone(), "/admin/write-page", request.clone())
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let restored = state.wiki.read_page(ws, proj, &path).unwrap();
    assert_eq!(restored.body, original.body);
    assert_eq!(restored.frontmatter, original.frontmatter);
    assert_eq!(
        state
            .reader
            .latest_page_id_by_ids(ws, proj, path.as_str().into())
            .await
            .unwrap(),
        original_id
    );
    conn.execute_batch("DROP TRIGGER refuse_metadata;").unwrap();
    assert_eq!(
        post_json(state.clone(), "/admin/write-page", request)
            .await
            .status(),
        StatusCode::OK
    );
    let recovered = state.wiki.read_page(ws, proj, &path).unwrap();
    assert_eq!(recovered.frontmatter["abstract"], "Refused summary.");
    assert!(recovered.body.contains("Replacement body."));
    assert_ne!(
        state
            .reader
            .latest_page_id_by_ids(ws, proj, path.as_str().into())
            .await
            .unwrap(),
        original_id
    );
}

/// The route `ai-memory write-page` posts to sets a profile entry's
/// `applies_to` and `enforced_by`.
#[tokio::test]
async fn write_page_sets_profile_fields() {
    let tmp = TempDir::new().unwrap();
    let state = make_state(&tmp).await;
    let request = json!({
        "workspace": "default", "project": "_global", "path": "profile/style/types.md",
        "body": "# Types\n\nNever cast.", "applies_to": ["TypeScript"],
        "enforced_by": "pre-push hook"
    });
    assert_eq!(
        post_json(state.clone(), "/admin/write-page", request)
            .await
            .status(),
        StatusCode::OK
    );
    let ws = state
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let global = state
        .writer
        .get_or_create_project(ws, "_global", None)
        .await
        .unwrap();
    let path = ai_memory_core::PagePath::new("profile/style/types.md").unwrap();
    let md = state.wiki.read_page(ws, global, &path).unwrap();
    assert_eq!(md.frontmatter["applies_to"], json!(["typescript"]));
    assert_eq!(md.frontmatter["enforced_by"], "pre-push hook");
}
