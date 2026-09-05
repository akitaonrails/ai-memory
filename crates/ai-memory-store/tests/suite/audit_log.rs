//! Reader for the append-only `audit_log` table.

use ai_memory_core::{NewPage, NewUser, PagePath, ProjectId, Tier, UserId, UserRole, WorkspaceId};
use ai_memory_store::{AuditLogFilter, Store};

async fn seed_page(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    path: &str,
    body: &str,
    author_id: Option<UserId>,
) {
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path).unwrap(),
            title: path.to_string(),
            body: body.to_string(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id,
            expires_at: None,
            entities: Vec::new(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn list_audit_events_resolves_names_pages_and_clamps_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    let alice = store
        .writer
        .create_human_user(
            NewUser {
                username: "alice".to_string(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();

    let default_ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let scratch = store
        .writer
        .get_or_create_project(default_ws, "scratch".to_string(), None)
        .await
        .unwrap();
    let other_ws = store
        .writer
        .get_or_create_workspace("other".to_string())
        .await
        .unwrap();
    let other_proj = store
        .writer
        .get_or_create_project(other_ws, "labs".to_string(), None)
        .await
        .unwrap();

    seed_page(
        &store,
        default_ws,
        scratch,
        "notes/alice.md",
        "attributed",
        Some(alice),
    )
    .await;
    seed_page(
        &store,
        default_ws,
        scratch,
        "notes/anon.md",
        "anonymous",
        None,
    )
    .await;
    seed_page(
        &store,
        other_ws,
        other_proj,
        "notes/elsewhere.md",
        "other scope",
        Some(alice),
    )
    .await;
    // Second write of the same path produces `supersede_page`.
    seed_page(
        &store,
        default_ws,
        scratch,
        "notes/alice.md",
        "attributed v2",
        Some(alice),
    )
    .await;

    let all = store
        .reader
        .list_audit_events(AuditLogFilter::default())
        .await
        .unwrap();
    assert!(all.len() >= 4, "create ×3 + supersede, got {}", all.len());
    assert!(
        all.windows(2).all(|w| w[0].id > w[1].id),
        "newest-first by id"
    );

    let alice_write = all
        .iter()
        .find(|e| e.page_path.as_deref() == Some("notes/alice.md") && e.op == "create_page")
        .expect("attributed create_page");
    assert_eq!(alice_write.workspace.as_deref(), Some("default"));
    assert_eq!(alice_write.project.as_deref(), Some("scratch"));
    assert_eq!(alice_write.author_username.as_deref(), Some("alice"));
    assert_eq!(alice_write.detail, "{}");
    assert!(alice_write.at > 0);

    let anon = all
        .iter()
        .find(|e| e.page_path.as_deref() == Some("notes/anon.md"))
        .expect("anonymous write");
    assert_eq!(anon.author_username, None, "anonymous write yields None");

    let scoped = store
        .reader
        .list_audit_events(AuditLogFilter {
            workspace: Some("default".to_string()),
            project: Some("scratch".to_string()),
            ..AuditLogFilter::default()
        })
        .await
        .unwrap();
    assert!(!scoped.is_empty());
    assert!(scoped.iter().all(|e| {
        e.workspace.as_deref() == Some("default") && e.project.as_deref() == Some("scratch")
    }));
    assert!(
        scoped
            .iter()
            .all(|e| e.page_path.as_deref() != Some("notes/elsewhere.md")),
        "scope filter must not leak the other workspace"
    );

    let supersedes = store
        .reader
        .list_audit_events(AuditLogFilter {
            op: Some("supersede_page".to_string()),
            ..AuditLogFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(supersedes.len(), 1);
    assert_eq!(supersedes[0].op, "supersede_page");

    let first = store
        .reader
        .list_audit_events(AuditLogFilter {
            limit: 2,
            ..AuditLogFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(first.len(), 2);
    let next = store
        .reader
        .list_audit_events(AuditLogFilter {
            before_id: Some(first[1].id),
            limit: 200,
            ..AuditLogFilter::default()
        })
        .await
        .unwrap();
    assert!(!next.is_empty());
    let first_ids: Vec<i64> = first.iter().map(|e| e.id).collect();
    assert!(
        next.iter()
            .all(|e| e.id < first[1].id && !first_ids.contains(&e.id)),
        "keyset must not skip or duplicate the first page"
    );

    let clamped_low = store
        .reader
        .list_audit_events(AuditLogFilter {
            limit: 0,
            ..AuditLogFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(clamped_low.len(), 1, "limit 0 clamps to 1");

    let clamped_high = store
        .reader
        .list_audit_events(AuditLogFilter {
            limit: 10_000,
            ..AuditLogFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(
        clamped_high.len(),
        all.len(),
        "limit 10000 clamps to 200 but we have fewer rows"
    );
    assert!(clamped_high.len() <= 200);
}
