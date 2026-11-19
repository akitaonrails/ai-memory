//! In-project link graph (`ReaderPool::project_link_graph`): latest
//! unexpired pages in one project and the resolved wikilinks among them.
//!
//! The invariants under test:
//! - Same-project resolved links become edges; isolated pages stay as nodes.
//! - Cross-project and unresolved links never appear.
//! - Expired pages are neither nodes nor endpoints.
//! - When a project is larger than `PROJECT_LINK_GRAPH_MAX_NODES`, high-degree
//!   pages are kept ahead of isolated ones, then recency.

use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_store::{PROJECT_LINK_GRAPH_MAX_NODES, Store};

fn page_with_links(
    ws: WorkspaceId,
    proj: ProjectId,
    path: &str,
    links: Vec<ai_memory_core::LinkTarget>,
) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: path.to_string(),
        body: "body".into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links,
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
        trust_class: None,
    }
}

fn same_project_link(path: &str) -> ai_memory_core::LinkTarget {
    ai_memory_core::LinkTarget {
        workspace: None,
        project: None,
        path: PagePath::new(path).unwrap(),
        relation: None,
    }
}

fn cross_project_link(project: &str, path: &str) -> ai_memory_core::LinkTarget {
    ai_memory_core::LinkTarget {
        workspace: None,
        project: Some(project.to_string()),
        path: PagePath::new(path).unwrap(),
        relation: None,
    }
}

fn with_expiry(mut page: NewPage, expires_at: jiff::Timestamp) -> NewPage {
    page.expires_at = Some(expires_at);
    page
}

async fn seeded() -> (tempfile::TempDir, Store, WorkspaceId, ProjectId) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let app = store
        .writer
        .get_or_create_project(ws, "app".to_string(), None)
        .await
        .unwrap();
    let lib = store
        .writer
        .get_or_create_project(ws, "lib".to_string(), None)
        .await
        .unwrap();

    store
        .writer
        .upsert_page(page_with_links(ws, lib, "notes/x.md", vec![]))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page_with_links(ws, app, "notes/b.md", vec![]))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page_with_links(
            ws,
            app,
            "notes/c.md",
            vec![same_project_link("notes/b.md")],
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page_with_links(
            ws,
            app,
            "notes/a.md",
            vec![
                same_project_link("notes/b.md"),
                same_project_link("notes/nope.md"),
                cross_project_link("lib", "notes/x.md"),
            ],
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page_with_links(ws, app, "notes/isolated.md", vec![]))
        .await
        .unwrap();

    (tmp, store, ws, app)
}

#[tokio::test]
async fn project_graph_keeps_same_project_edges_and_isolated_pages() {
    let (_tmp, store, ws, app) = seeded().await;
    let graph = store.reader.project_link_graph(ws, app).await.unwrap();

    let mut paths: Vec<&str> = graph.nodes.iter().map(|n| n.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec![
            "notes/a.md",
            "notes/b.md",
            "notes/c.md",
            "notes/isolated.md",
        ]
    );
    assert!(!graph.truncated);

    let mut pairs: Vec<(&str, &str)> = graph
        .edges
        .iter()
        .map(|e| (e.from_path.as_str(), e.to_path.as_str()))
        .collect();
    pairs.sort_unstable();
    assert_eq!(
        pairs,
        vec![("notes/a.md", "notes/b.md"), ("notes/c.md", "notes/b.md")]
    );

    let a = graph.nodes.iter().find(|n| n.path == "notes/a.md").unwrap();
    let b = graph.nodes.iter().find(|n| n.path == "notes/b.md").unwrap();
    let isolated = graph
        .nodes
        .iter()
        .find(|n| n.path == "notes/isolated.md")
        .unwrap();
    assert_eq!(a.degree, 1);
    assert_eq!(b.degree, 2, "b is cited by a and c");
    assert_eq!(isolated.degree, 0);
}

#[tokio::test]
async fn project_graph_omits_cross_project_and_unresolved_links() {
    let (_tmp, store, ws, app) = seeded().await;
    let graph = store.reader.project_link_graph(ws, app).await.unwrap();
    let blob = format!("{graph:?}");
    assert!(
        !blob.contains("notes/x.md"),
        "cross-project target must not appear: {blob}"
    );
    assert!(
        !blob.contains("notes/nope.md"),
        "unresolved target must not appear: {blob}"
    );
}

#[tokio::test]
async fn project_graph_hides_expired_pages_and_their_edges() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let app = store
        .writer
        .get_or_create_project(ws, "app".to_string(), None)
        .await
        .unwrap();
    let expired_at: jiff::Timestamp = "2000-01-01T00:00:00Z".parse().unwrap();
    let future_at: jiff::Timestamp = "2099-01-01T00:00:00Z".parse().unwrap();

    for page in [
        page_with_links(ws, app, "notes/live.md", vec![]),
        with_expiry(
            page_with_links(ws, app, "notes/expired.md", vec![]),
            expired_at,
        ),
        with_expiry(
            page_with_links(ws, app, "notes/future.md", vec![]),
            future_at,
        ),
        page_with_links(
            ws,
            app,
            "notes/hub.md",
            vec![
                same_project_link("notes/live.md"),
                same_project_link("notes/expired.md"),
                same_project_link("notes/future.md"),
            ],
        ),
        with_expiry(
            page_with_links(
                ws,
                app,
                "notes/expired_from.md",
                vec![same_project_link("notes/live.md")],
            ),
            expired_at,
        ),
    ] {
        store.writer.upsert_page(page).await.unwrap();
    }

    let graph = store.reader.project_link_graph(ws, app).await.unwrap();
    let mut paths: Vec<&str> = graph.nodes.iter().map(|n| n.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec!["notes/future.md", "notes/hub.md", "notes/live.md"]
    );
    let mut pairs: Vec<(&str, &str)> = graph
        .edges
        .iter()
        .map(|e| (e.from_path.as_str(), e.to_path.as_str()))
        .collect();
    pairs.sort_unstable();
    assert_eq!(
        pairs,
        vec![
            ("notes/hub.md", "notes/future.md"),
            ("notes/hub.md", "notes/live.md"),
        ]
    );
}

#[tokio::test]
async fn project_graph_caps_by_degree_then_recency() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let app = store
        .writer
        .get_or_create_project(ws, "app".to_string(), None)
        .await
        .unwrap();

    store
        .writer
        .upsert_page(page_with_links(ws, app, "notes/leaf.md", vec![]))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page_with_links(
            ws,
            app,
            "notes/hub.md",
            vec![same_project_link("notes/leaf.md")],
        ))
        .await
        .unwrap();

    let isolated = PROJECT_LINK_GRAPH_MAX_NODES;
    for i in 0..isolated {
        store
            .writer
            .upsert_page(page_with_links(
                ws,
                app,
                &format!("notes/iso-{i:03}.md"),
                vec![],
            ))
            .await
            .unwrap();
    }

    let graph = store.reader.project_link_graph(ws, app).await.unwrap();
    assert!(graph.truncated);
    assert_eq!(graph.nodes.len(), PROJECT_LINK_GRAPH_MAX_NODES);
    let paths: Vec<&str> = graph.nodes.iter().map(|n| n.path.as_str()).collect();
    assert!(
        paths.contains(&"notes/hub.md") && paths.contains(&"notes/leaf.md"),
        "linked pages outrank isolated ones: {paths:?}"
    );
    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.from_path == "notes/hub.md" && e.to_path == "notes/leaf.md"),
        "the kept edge must survive the cap: {:?}",
        graph.edges
    );
}
