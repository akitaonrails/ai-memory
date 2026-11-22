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
//! - **The walk honours per-project authorization (#708):** a viewer never
//!   sees, nor walks through, a page in a project they cannot read.
//! - **The walk honours page TTL:** an expired neighbour is neither
//!   returned nor walked through. The seed lookup is an exact-path read
//!   and still starts from an expired page.

use ai_memory_core::{NewPage, NewUser, PagePath, ProjectId, Relation, Tier, UserId, WorkspaceId};
use ai_memory_store::{
    AccessMode, GrantLevel, RELATED_WALK_MAX_DEPTH, RELATED_WALK_MAX_NODES, Store,
};

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

fn contradiction_link(path: &str) -> ai_memory_core::LinkTarget {
    ai_memory_core::LinkTarget {
        workspace: None,
        project: None,
        path: PagePath::new(path).unwrap(),
        relation: Some(Relation::Contradicts),
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
        .related_walk(ws, app, "notes/a.md".into(), 1, None)
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
        .related_walk(ws, app, "notes/a.md".into(), 2, None)
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
        .related_walk(ws, app, "notes/a.md".into(), RELATED_WALK_MAX_DEPTH, None)
        .await
        .unwrap();
    // Anything past the cap must behave exactly like the cap, not walk further.
    let over = store
        .reader
        .related_walk(ws, app, "notes/a.md".into(), 100, None)
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
        .related_walk(ws, app, "notes/a.md".into(), RELATED_WALK_MAX_DEPTH, None)
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
        .related_walk(ws, app, "hub.md".into(), 1, None)
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
        .related_walk(ws, app, "notes/does-not-exist.md".into(), 2, None)
        .await
        .unwrap();
    assert!(nodes.is_empty(), "a missing seed yields no related pages");
}

/// Under per-project authorization the walk shows only what the viewer may
/// read and never walks through what they may not, in either direction: a
/// page in a restricted project without a grant is absent, and so is a
/// readable page reachable only through it.
///
/// ```text
///   app:a ──▶ lib:x                  (lib restricted, viewer holds a grant)
///   app:a ──▶ secret:s ──▶ lib:y     (secret restricted, no grant; lib:y only via s)
///   secret:t ──▶ app:a               (incoming edge from the unreadable project)
/// ```
#[tokio::test]
async fn the_walk_hides_and_does_not_cross_projects_the_viewer_cannot_read() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let mut projects = Vec::new();
    for name in ["app", "lib", "secret"] {
        projects.push(
            store
                .writer
                .get_or_create_project(ws, name.to_string(), None)
                .await
                .unwrap(),
        );
    }
    let (app, lib, secret) = (projects[0], projects[1], projects[2]);
    for page in [
        page_with_links(ws, lib, "notes/x.md", vec![]),
        page_with_links(ws, lib, "notes/y.md", vec![]),
        page_with_links(
            ws,
            secret,
            "notes/s.md",
            vec![cross_project_link("lib", "notes/y.md")],
        ),
        page_with_links(
            ws,
            app,
            "notes/a.md",
            vec![
                cross_project_link("lib", "notes/x.md"),
                cross_project_link("secret", "notes/s.md"),
            ],
        ),
        page_with_links(
            ws,
            secret,
            "notes/t.md",
            vec![cross_project_link("app", "notes/a.md")],
        ),
    ] {
        store.writer.upsert_page(page).await.unwrap();
    }
    for project in [lib, secret] {
        store
            .writer
            .set_access_mode(project, AccessMode::Restricted)
            .await
            .unwrap();
    }
    let viewer = store
        .writer
        .create_human_user(
            NewUser {
                username: "vera".into(),
                name: None,
                email: None,
            },
            ai_memory_core::UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    store
        .writer
        .grant_memory(viewer, lib, GrantLevel::Read, None)
        .await
        .unwrap();

    let walk = |who: Option<UserId>| {
        let reader = store.reader.clone();
        async move {
            let mut seen: Vec<String> = reader
                .related_walk(ws, app, "notes/a.md".into(), RELATED_WALK_MAX_DEPTH, who)
                .await
                .unwrap()
                .into_iter()
                .map(|n| format!("{}:{}", n.page.project, n.page.path))
                .collect();
            seen.sort();
            seen
        }
    };

    assert_eq!(
        walk(Some(viewer)).await,
        vec!["lib:notes/x.md".to_string()],
        "secret:s and secret:t are unreadable, and lib:y is reachable only through s"
    );
    // Control: no viewer (root, or authorization off) walks the whole graph.
    assert_eq!(
        walk(None).await,
        vec![
            "lib:notes/x.md".to_string(),
            "lib:notes/y.md".to_string(),
            "secret:notes/s.md".to_string(),
            "secret:notes/t.md".to_string(),
        ]
    );
}

fn expired_at() -> jiff::Timestamp {
    "2000-01-01T00:00:00Z".parse().unwrap()
}

fn future_at() -> jiff::Timestamp {
    "2099-01-01T00:00:00Z".parse().unwrap()
}

fn with_expiry(mut page: NewPage, expires_at: jiff::Timestamp) -> NewPage {
    page.expires_at = Some(expires_at);
    page
}

/// Expired neighbours stay off the retrieval graph the same way #708
/// hides unreadable projects: they are not returned, and a live page
/// reachable only through one is not returned either. A live neighbour
/// and a neighbour whose TTL is still in the future remain visible.
///
/// ```text
///   live.md ◀── a.md ──▶ expired.md ──▶ only_via_expired.md
///                 │
///                 └──▶ future.md
///   live_in.md ──▶ a.md ◀── expired_in.md
/// ```
#[tokio::test]
async fn the_walk_hides_and_does_not_cross_expired_neighbours() {
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

    for page in [
        page_with_links(ws, app, "notes/live.md", vec![]),
        page_with_links(ws, app, "notes/only_via_expired.md", vec![]),
        page_with_links(
            ws,
            app,
            "notes/live_in.md",
            vec![same_project_link("notes/a.md")],
        ),
        with_expiry(
            page_with_links(
                ws,
                app,
                "notes/expired.md",
                vec![same_project_link("notes/only_via_expired.md")],
            ),
            expired_at(),
        ),
        with_expiry(
            page_with_links(
                ws,
                app,
                "notes/expired_in.md",
                vec![same_project_link("notes/a.md")],
            ),
            expired_at(),
        ),
        with_expiry(
            page_with_links(ws, app, "notes/future.md", vec![]),
            future_at(),
        ),
        page_with_links(
            ws,
            app,
            "notes/a.md",
            vec![
                same_project_link("notes/live.md"),
                same_project_link("notes/expired.md"),
                same_project_link("notes/future.md"),
            ],
        ),
    ] {
        store.writer.upsert_page(page).await.unwrap();
    }

    let paths = |depth: u8| {
        let reader = store.reader.clone();
        async move {
            let mut seen: Vec<String> = reader
                .related_walk(ws, app, "notes/a.md".into(), depth, None)
                .await
                .unwrap()
                .into_iter()
                .map(|n| n.page.path)
                .collect();
            seen.sort();
            seen
        }
    };

    assert_eq!(
        paths(1).await,
        vec![
            "notes/future.md".to_string(),
            "notes/live.md".to_string(),
            "notes/live_in.md".to_string(),
        ],
        "expired outgoing and incoming neighbours are hidden; live and future TTL stay"
    );
    assert_eq!(
        paths(RELATED_WALK_MAX_DEPTH).await,
        vec![
            "notes/future.md".to_string(),
            "notes/live.md".to_string(),
            "notes/live_in.md".to_string(),
        ],
        "only_via_expired is reachable only through expired.md, so it stays hidden"
    );

    // Exact-path seed lookup still starts from an expired page; its live
    // neighbour is then a retrieval hop and is returned.
    let mut from_expired: Vec<String> = store
        .reader
        .related_walk(ws, app, "notes/expired.md".into(), 1, None)
        .await
        .unwrap()
        .into_iter()
        .map(|n| n.page.path)
        .collect();
    from_expired.sort();
    assert_eq!(
        from_expired,
        vec![
            "notes/a.md".to_string(),
            "notes/only_via_expired.md".to_string(),
        ],
        "walking from an expired seed still returns its live neighbours"
    );
}

/// `page_links` is the single-hop primitive the web link panel calls.
/// Same TTL gate as `related_walk`: expired far ends are omitted, live
/// and future-TTL far ends remain, in both directions.
#[tokio::test]
async fn page_links_hides_expired_neighbours() {
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

    for page in [
        page_with_links(ws, app, "notes/live.md", vec![]),
        page_with_links(
            ws,
            app,
            "notes/live_in.md",
            vec![same_project_link("notes/a.md")],
        ),
        with_expiry(
            page_with_links(ws, app, "notes/expired.md", vec![]),
            expired_at(),
        ),
        with_expiry(
            page_with_links(
                ws,
                app,
                "notes/expired_in.md",
                vec![same_project_link("notes/a.md")],
            ),
            expired_at(),
        ),
        with_expiry(
            page_with_links(ws, app, "notes/future.md", vec![]),
            future_at(),
        ),
        page_with_links(
            ws,
            app,
            "notes/a.md",
            vec![
                same_project_link("notes/live.md"),
                same_project_link("notes/expired.md"),
                same_project_link("notes/future.md"),
            ],
        ),
    ] {
        store.writer.upsert_page(page).await.unwrap();
    }

    let links = store
        .reader
        .page_links(ws, app, "notes/a.md".into(), None)
        .await
        .unwrap();
    let mut outgoing: Vec<&str> = links.links.iter().map(|p| p.path.as_str()).collect();
    outgoing.sort_unstable();
    let mut incoming: Vec<&str> = links.backlinks.iter().map(|p| p.path.as_str()).collect();
    incoming.sort_unstable();
    assert_eq!(
        outgoing,
        vec!["notes/future.md", "notes/live.md"],
        "expired outgoing neighbour is hidden; live and future TTL stay"
    );
    assert_eq!(
        incoming,
        vec!["notes/live_in.md"],
        "expired incoming neighbour is hidden; live backlink stays"
    );
}

/// The cross-project graph (`/api/v1/graph`, the web graph view) hides an
/// edge whose either end is expired, like every other retrieval surface:
/// an edge names both pages' paths. A live edge and one into a page whose
/// TTL is still in the future stay visible, scoped or not.
#[tokio::test]
async fn cross_project_edges_hide_edges_touching_an_expired_page() {
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
    let infra = store
        .writer
        .get_or_create_project(ws, "infra".to_string(), None)
        .await
        .unwrap();

    for page in [
        page_with_links(ws, infra, "runbooks/live.md", vec![]),
        with_expiry(
            page_with_links(ws, infra, "runbooks/expired.md", vec![]),
            expired_at(),
        ),
        with_expiry(
            page_with_links(ws, infra, "runbooks/future.md", vec![]),
            future_at(),
        ),
        page_with_links(
            ws,
            app,
            "notes/to_all.md",
            vec![
                cross_project_link("infra", "runbooks/live.md"),
                cross_project_link("infra", "runbooks/expired.md"),
                cross_project_link("infra", "runbooks/future.md"),
            ],
        ),
        with_expiry(
            page_with_links(
                ws,
                app,
                "notes/expired_from.md",
                vec![cross_project_link("infra", "runbooks/live.md")],
            ),
            expired_at(),
        ),
    ] {
        store.writer.upsert_page(page).await.unwrap();
    }

    for scope in [None, Some((ws, app))] {
        let edges = store.reader.cross_project_edges(scope, None).await.unwrap();
        let mut pairs: Vec<(String, String)> = edges
            .into_iter()
            .map(|e| (e.from_path, e.to_path))
            .collect();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                (
                    "notes/to_all.md".to_string(),
                    "runbooks/future.md".to_string()
                ),
                (
                    "notes/to_all.md".to_string(),
                    "runbooks/live.md".to_string()
                ),
            ],
            "scope {scope:?}: edges touching an expired page must be hidden"
        );
    }
}

/// `page_links_preview` cuts each direction in SQL and counts the rest under
/// the same filters: a back-link from a project the viewer cannot read is
/// neither listed nor counted, so "and N more" leaks nothing either.
///
/// ```text
///   app:b1..b5 ──▶ app:a ◀── secret:s1..s3   (secret restricted, no grant)
/// ```
#[tokio::test]
async fn page_links_preview_caps_in_sql_and_counts_only_readable_neighbours() {
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
    let secret = store
        .writer
        .get_or_create_project(ws, "secret".to_string(), None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page_with_links(ws, app, "notes/a.md", vec![]))
        .await
        .unwrap();
    for i in 1..=5 {
        let path = format!("notes/b{i}.md");
        let page = page_with_links(ws, app, &path, vec![same_project_link("notes/a.md")]);
        store.writer.upsert_page(page).await.unwrap();
    }
    for i in 1..=3 {
        let path = format!("notes/s{i}.md");
        let page = page_with_links(
            ws,
            secret,
            &path,
            vec![cross_project_link("app", "notes/a.md")],
        );
        store.writer.upsert_page(page).await.unwrap();
    }
    store
        .writer
        .set_access_mode(secret, AccessMode::Restricted)
        .await
        .unwrap();
    let viewer = store
        .writer
        .create_human_user(
            NewUser {
                username: "vera".into(),
                name: None,
                email: None,
            },
            ai_memory_core::UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();

    let preview = |who: Option<UserId>, limit: usize| {
        let reader = store.reader.clone();
        async move {
            reader
                .page_links_preview(ws, app, "notes/a.md".into(), who, limit)
                .await
                .unwrap()
        }
    };

    let cut = preview(Some(viewer), 2).await;
    let paths: Vec<&str> = cut.backlinks.iter().map(|p| p.path.as_str()).collect();
    assert_eq!(paths, ["notes/b1.md", "notes/b2.md"]);
    assert_eq!(
        cut.backlinks_total, 5,
        "the secret back-links are not counted"
    );
    assert!(cut.links.is_empty() && cut.links_total == 0);

    let all = preview(Some(viewer), 10).await;
    assert_eq!(all.backlinks.len(), 5);
    assert_eq!(all.backlinks_total, 5);
    assert!(all.backlinks.iter().all(|p| p.project == "app"));

    // Control: no viewer (root, or authorization off) counts all eight, and
    // the cut keeps `page_links` order.
    let root = preview(None, 2).await;
    assert_eq!(root.backlinks_total, 8);
    let full = store
        .reader
        .page_links(ws, app, "notes/a.md".into(), None)
        .await
        .unwrap();
    assert_eq!(full.backlinks.len(), 8);
    let head: Vec<&str> = full.backlinks[..2]
        .iter()
        .map(|p| p.path.as_str())
        .collect();
    let cut_root: Vec<&str> = root.backlinks.iter().map(|p| p.path.as_str()).collect();
    assert_eq!(cut_root, head);
}

/// `contradiction_edges` returns declared contradicts edges from latest
/// unexpired pages. A target edge resolves only when pointing at an existing,
/// latest, and unexpired page. If the target was tombstoned, expired, or never
/// existed, `resolved` evaluates to false. An expired source page is omitted
/// entirely from contradiction edges.
#[tokio::test]
async fn contradiction_edges_require_latest_unexpired_target() {
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
        .upsert_page(page_with_links(ws, app, "notes/live_target.md", vec![]))
        .await
        .unwrap();

    let tombstoned_target_id = store
        .writer
        .upsert_page(page_with_links(
            ws,
            app,
            "notes/tombstoned_target.md",
            vec![],
        ))
        .await
        .unwrap();

    store
        .writer
        .upsert_page(with_expiry(
            page_with_links(ws, app, "notes/expired_target.md", vec![]),
            expired_at(),
        ))
        .await
        .unwrap();

    store
        .writer
        .upsert_page(with_expiry(
            page_with_links(ws, app, "notes/future_target.md", vec![]),
            future_at(),
        ))
        .await
        .unwrap();

    store
        .writer
        .upsert_page(page_with_links(
            ws,
            app,
            "notes/source.md",
            vec![
                contradiction_link("notes/live_target.md"),
                contradiction_link("notes/tombstoned_target.md"),
                contradiction_link("notes/expired_target.md"),
                contradiction_link("notes/future_target.md"),
                contradiction_link("notes/missing_target.md"),
            ],
        ))
        .await
        .unwrap();

    store
        .writer
        .upsert_page(with_expiry(
            page_with_links(
                ws,
                app,
                "notes/expired_source.md",
                vec![contradiction_link("notes/live_target.md")],
            ),
            expired_at(),
        ))
        .await
        .unwrap();

    let soft_deleted = store
        .writer
        .soft_delete_for_decay_if_latest(
            ws,
            app,
            PagePath::new("notes/tombstoned_target.md").unwrap(),
            tombstoned_target_id,
        )
        .await
        .unwrap();
    assert!(soft_deleted, "tombstone soft delete succeeded");

    let edges = store.reader.contradiction_edges(ws, app).await.unwrap();

    assert!(
        !edges
            .iter()
            .any(|e| e.from_path == "notes/expired_source.md"),
        "expired source page must not emit contradiction edges"
    );

    let pairs: Vec<(String, String, bool)> = edges
        .into_iter()
        .map(|e| (e.from_path, e.to_path, e.resolved))
        .collect();

    assert_eq!(
        pairs,
        vec![
            (
                "notes/source.md".to_string(),
                "notes/expired_target.md".to_string(),
                false,
            ),
            (
                "notes/source.md".to_string(),
                "notes/future_target.md".to_string(),
                true,
            ),
            (
                "notes/source.md".to_string(),
                "notes/live_target.md".to_string(),
                true,
            ),
            (
                "notes/source.md".to_string(),
                "notes/missing_target.md".to_string(),
                false,
            ),
            (
                "notes/source.md".to_string(),
                "notes/tombstoned_target.md".to_string(),
                false,
            ),
        ]
    );
}

/// `dangling_cross_project_links` returns declared dependencies on pages in
/// another project that do not resolve to an existing, latest, unexpired page.
/// If the target was tombstoned (soft-deleted), expired, or never existed, it
/// is reported as dangling. Targets that are live or have future TTL do not
/// dangle. An expired source page is omitted entirely.
#[tokio::test]
async fn dangling_cross_project_links_require_latest_unexpired_target() {
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
        .upsert_page(page_with_links(ws, lib, "notes/live_target.md", vec![]))
        .await
        .unwrap();

    let tombstoned_target_id = store
        .writer
        .upsert_page(page_with_links(
            ws,
            lib,
            "notes/tombstoned_target.md",
            vec![],
        ))
        .await
        .unwrap();

    store
        .writer
        .upsert_page(with_expiry(
            page_with_links(ws, lib, "notes/expired_target.md", vec![]),
            expired_at(),
        ))
        .await
        .unwrap();

    store
        .writer
        .upsert_page(with_expiry(
            page_with_links(ws, lib, "notes/future_target.md", vec![]),
            future_at(),
        ))
        .await
        .unwrap();

    store
        .writer
        .upsert_page(page_with_links(
            ws,
            app,
            "notes/source.md",
            vec![
                cross_project_link("ghost", "notes/missing.md"),
                cross_project_link("lib", "notes/live_target.md"),
                cross_project_link("lib", "notes/tombstoned_target.md"),
                cross_project_link("lib", "notes/expired_target.md"),
                cross_project_link("lib", "notes/future_target.md"),
                cross_project_link("lib", "notes/missing_target.md"),
            ],
        ))
        .await
        .unwrap();

    store
        .writer
        .upsert_page(with_expiry(
            page_with_links(
                ws,
                app,
                "notes/expired_source.md",
                vec![cross_project_link("lib", "notes/live_target.md")],
            ),
            expired_at(),
        ))
        .await
        .unwrap();

    let soft_deleted = store
        .writer
        .soft_delete_for_decay_if_latest(
            ws,
            lib,
            PagePath::new("notes/tombstoned_target.md").unwrap(),
            tombstoned_target_id,
        )
        .await
        .unwrap();
    assert!(soft_deleted, "tombstone soft delete succeeded");

    let dangling = store
        .reader
        .dangling_cross_project_links(ws, app)
        .await
        .unwrap();

    assert!(
        !dangling
            .iter()
            .any(|d| d.from_path == "notes/expired_source.md"),
        "expired source page must not emit dangling links"
    );

    let summary: Vec<(String, String, String, bool)> = dangling
        .into_iter()
        .map(|d| (d.from_path, d.project, d.path, d.project_exists))
        .collect();

    assert_eq!(
        summary,
        vec![
            (
                "notes/source.md".to_string(),
                "ghost".to_string(),
                "notes/missing.md".to_string(),
                false,
            ),
            (
                "notes/source.md".to_string(),
                "lib".to_string(),
                "notes/expired_target.md".to_string(),
                true,
            ),
            (
                "notes/source.md".to_string(),
                "lib".to_string(),
                "notes/missing_target.md".to_string(),
                true,
            ),
            (
                "notes/source.md".to_string(),
                "lib".to_string(),
                "notes/tombstoned_target.md".to_string(),
                true,
            ),
        ]
    );
}

#[tokio::test]
async fn cross_project_degree_ignores_expired_pages() {
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
    let other = store
        .writer
        .get_or_create_project(ws, "other".to_string(), None)
        .await
        .unwrap();

    // In lib: one live page and one expired page.
    store
        .writer
        .upsert_page(page_with_links(ws, lib, "notes/live_target.md", vec![]))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(with_expiry(
            page_with_links(ws, lib, "notes/expired_target.md", vec![]),
            expired_at(),
        ))
        .await
        .unwrap();

    // In app: one live page linking to lib (both live and expired targets) and
    // one expired page linking to other.
    store
        .writer
        .upsert_page(page_with_links(
            ws,
            app,
            "notes/source.md",
            vec![
                cross_project_link("lib", "notes/live_target.md"),
                cross_project_link("lib", "notes/expired_target.md"),
            ],
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(with_expiry(
            page_with_links(
                ws,
                app,
                "notes/expired_source.md",
                vec![cross_project_link("other", "notes/other_target.md")],
            ),
            expired_at(),
        ))
        .await
        .unwrap();

    // In other: one live target, one live page linking to app, and one expired page linking to app.
    store
        .writer
        .upsert_page(page_with_links(ws, other, "notes/other_target.md", vec![]))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page_with_links(
            ws,
            other,
            "notes/other_live_source.md",
            vec![cross_project_link("app", "notes/source.md")],
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(with_expiry(
            page_with_links(
                ws,
                other,
                "notes/other_expired_source.md",
                vec![cross_project_link("app", "notes/source.md")],
            ),
            expired_at(),
        ))
        .await
        .unwrap();

    let app_brief = store
        .reader
        .briefing_for_project(ws, app, 5, ai_memory_core::OwnerFilter::Any, false)
        .await
        .unwrap();
    assert_eq!(
        app_brief.cross_project_dependencies, 1,
        "app depends only on lib (live target); expired target and expired source to other are omitted"
    );
    assert_eq!(
        app_brief.cross_project_dependents, 1,
        "only other (live source) depends on app; expired source in other is omitted"
    );

    let lib_brief = store
        .reader
        .briefing_for_project(ws, lib, 5, ai_memory_core::OwnerFilter::Any, false)
        .await
        .unwrap();
    assert_eq!(lib_brief.cross_project_dependents, 1, "app depends on lib");
    assert_eq!(lib_brief.cross_project_dependencies, 0);

    let other_brief = store
        .reader
        .briefing_for_project(ws, other, 5, ai_memory_core::OwnerFilter::Any, false)
        .await
        .unwrap();
    assert_eq!(
        other_brief.cross_project_dependencies, 1,
        "other depends on app"
    );
    assert_eq!(
        other_brief.cross_project_dependents, 0,
        "app's expired link to other does not make other a dependency of app"
    );
}
