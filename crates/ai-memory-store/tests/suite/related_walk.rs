//! Bounded related-pages graph walk (`ReaderPool::related_walk`): a
//! breadth-first traversal over the resolved link graph, starting from one
//! seed page and following both outgoing links and incoming back-links out to
//! a requested hop depth. It is the multi-hop generalisation of the
//! single-hop `page_links` primitive.
//!
//! The invariants under test:
//! - **Depth controls reach.** Depth 1 returns only direct neighbours; depth 2
//!   also returns their neighbours; and so on.
//! - **The depth is hard-capped** at `RELATED_WALK_MAX_DEPTH`; a larger request
//!   is clamped, never honoured.
//! - **A global visited set makes the walk dedup- and cycle-safe:** no page is
//!   returned twice and no cycle loops forever; the seed itself is never in the
//!   result.
//! - **A total-node cap** bounds the response regardless of depth.
//! - **Cross-project links resolve** and carry their real workspace/project.

use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_store::{RELATED_WALK_MAX_DEPTH, RELATED_WALK_MAX_NODES, Store};

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

/// Seeds the small graph:
///
/// ```text
///   d ── links to ──▶ a ── links to ──▶ b ── links to ──▶ c
///                                        ▲                 │
///                                        └── links to ─────┘ (cycle b⇄c)
///                                        └── links to ──▶ lib:x   (cross-project)
/// ```
///
/// From `a`: depth 1 = {b (outgoing), d (incoming)}; depth 2 adds {c, lib:x}.
/// The `c → b` edge closes a cycle without shortening any distance from `a`.
async fn seeded_graph() -> (tempfile::TempDir, Store, WorkspaceId, ProjectId) {
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

    // Cross-project target first so the link resolves at write time.
    store
        .writer
        .upsert_page(page_with_links(ws, lib, "notes/x.md", vec![]))
        .await
        .unwrap();

    // Links back-resolve when their target lands, so creation order is free.
    store
        .writer
        .upsert_page(page_with_links(
            ws,
            app,
            "notes/b.md",
            vec![
                same_project_link("notes/c.md"),
                cross_project_link("lib", "notes/x.md"),
            ],
        ))
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
            vec![same_project_link("notes/b.md")],
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page_with_links(
            ws,
            app,
            "notes/d.md",
            vec![same_project_link("notes/a.md")],
        ))
        .await
        .unwrap();

    (tmp, store, ws, app)
}

#[tokio::test]
async fn depth_one_returns_only_direct_neighbours() {
    let (_tmp, store, ws, app) = seeded_graph().await;

    let nodes = store
        .reader
        .related_walk(ws, app, "notes/a.md".into(), 1)
        .await
        .unwrap();

    let mut paths: Vec<&str> = nodes.iter().map(|n| n.page.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec!["notes/b.md", "notes/d.md"],
        "depth 1 is direct neighbours only (outgoing b, incoming d)"
    );
    for n in &nodes {
        assert_eq!(n.depth, 1, "every depth-1 node is one hop away: {n:?}");
    }
    let b = nodes.iter().find(|n| n.page.path == "notes/b.md").unwrap();
    assert_eq!(b.direction, "link", "b is reached as an outgoing link");
    let d = nodes.iter().find(|n| n.page.path == "notes/d.md").unwrap();
    assert_eq!(
        d.direction, "backlink",
        "d is reached as an incoming back-link"
    );
}

#[tokio::test]
async fn depth_two_adds_second_hop_including_cross_project() {
    let (_tmp, store, ws, app) = seeded_graph().await;

    let nodes = store
        .reader
        .related_walk(ws, app, "notes/a.md".into(), 2)
        .await
        .unwrap();

    let mut paths: Vec<&str> = nodes.iter().map(|n| n.page.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec!["notes/b.md", "notes/c.md", "notes/d.md", "notes/x.md"],
        "depth 2 adds c (via b) and the cross-project lib:x (via b)"
    );

    let c = nodes.iter().find(|n| n.page.path == "notes/c.md").unwrap();
    assert_eq!(c.depth, 2, "c is two hops from a");

    let x = nodes.iter().find(|n| n.page.path == "notes/x.md").unwrap();
    assert_eq!(x.depth, 2, "the cross-project neighbour is two hops from a");
    assert_eq!(
        x.page.project, "lib",
        "cross-project link resolves to its real project"
    );
    assert_eq!(x.page.workspace, "default");
}

#[tokio::test]
async fn depth_is_clamped_to_the_hard_cap() {
    let (_tmp, store, ws, app) = seeded_graph().await;

    let capped = store
        .reader
        .related_walk(ws, app, "notes/a.md".into(), RELATED_WALK_MAX_DEPTH)
        .await
        .unwrap();
    // Anything past the cap must behave exactly like the cap, not walk further.
    let over = store
        .reader
        .related_walk(ws, app, "notes/a.md".into(), 100)
        .await
        .unwrap();

    let paths = |ns: &[ai_memory_store::RelatedNode]| {
        let mut v: Vec<String> = ns.iter().map(|n| n.page.path.clone()).collect();
        v.sort_unstable();
        v
    };
    assert_eq!(
        paths(&over),
        paths(&capped),
        "a request beyond RELATED_WALK_MAX_DEPTH is clamped to the cap"
    );
}

#[tokio::test]
async fn walk_is_dedup_and_cycle_safe() {
    let (_tmp, store, ws, app) = seeded_graph().await;

    // b⇄c is a cycle. A deep walk must terminate, never return the
    // seed, and never return a page twice.
    let nodes = store
        .reader
        .related_walk(ws, app, "notes/a.md".into(), RELATED_WALK_MAX_DEPTH)
        .await
        .unwrap();

    let mut seen = std::collections::HashSet::new();
    for n in &nodes {
        assert!(
            seen.insert(n.page.path.clone()),
            "page {} returned twice",
            n.page.path
        );
        assert_ne!(
            n.page.path, "notes/a.md",
            "the seed is never in its own related set"
        );
    }
}

#[tokio::test]
async fn total_node_cap_bounds_a_dense_hub() {
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

    // A hub linking to far more than the cap's worth of direct neighbours.
    let spoke_count = RELATED_WALK_MAX_NODES + 10;
    let mut links = Vec::new();
    for i in 0..spoke_count {
        let path = format!("spokes/s{i}.md");
        store
            .writer
            .upsert_page(page_with_links(ws, app, &path, vec![]))
            .await
            .unwrap();
        links.push(same_project_link(&path));
    }
    store
        .writer
        .upsert_page(page_with_links(ws, app, "hub.md", links))
        .await
        .unwrap();

    let nodes = store
        .reader
        .related_walk(ws, app, "hub.md".into(), 1)
        .await
        .unwrap();

    assert_eq!(
        nodes.len(),
        RELATED_WALK_MAX_NODES,
        "the total-node cap bounds the walk even when a hub has more neighbours"
    );
    let unique: std::collections::HashSet<_> = nodes.iter().map(|n| &n.page.path).collect();
    assert_eq!(
        unique.len(),
        nodes.len(),
        "capped result still has no duplicates"
    );
}

#[tokio::test]
async fn missing_seed_returns_empty() {
    let (_tmp, store, ws, app) = seeded_graph().await;
    let nodes = store
        .reader
        .related_walk(ws, app, "notes/does-not-exist.md".into(), 2)
        .await
        .unwrap();
    assert!(nodes.is_empty(), "a missing seed yields no related pages");
}
