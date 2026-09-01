//! One operator, one project, two machines.
//!
//! Project identity is derived from the checkout's *name*, never its absolute
//! path, which is what makes the same project portable between machines. This
//! lives in `ai-memory-consolidate` because that crate owns the name
//! derivation and also depends on the store, so the property can be asserted
//! end to end rather than in two halves that never meet.
//!
//! See `AGENTS.md` invariant 16.

use ai_memory_consolidate::{ProjectNameStrategy, derive_project_name};
use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_store::Store;

fn page(ws: WorkspaceId, proj: ProjectId, path: &str, title: &str, body: &str) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: title.into(),
        body: body.into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
    }
}

/// The scenario the maintainer named first: one operator, the same project,
/// two machines.
///
/// Project identity is derived from the checkout's *name*, never its absolute
/// path — so `/home/alice/work/proj-b` on one machine and
/// `/Users/alice/src/proj-b` on another resolve to one project row, and each
/// machine reads what the other wrote.
///
/// This is what makes the capability portable, and it is a property of name
/// derivation rather than of any storage code, so it is asserted end to end:
/// resolve both paths, then prove the second machine reads the first's page.
#[tokio::test]
async fn the_same_checkout_on_two_machines_resolves_to_one_project() {
    let machine_one = std::path::Path::new("/home/alice/work/proj-b");
    let machine_two = std::path::Path::new("/Users/alice/src/proj-b");

    let (name_one, _) = derive_project_name(machine_one, ProjectNameStrategy::Basename)
        .expect("a real checkout path resolves to a name");
    let (name_two, _) = derive_project_name(machine_two, ProjectNameStrategy::Basename)
        .expect("a real checkout path resolves to a name");
    assert_eq!(
        name_one, name_two,
        "the same project checked out at different absolute paths must derive \
         the same project name, or the two machines silently diverge"
    );

    // …and that name lands both machines in one project row, sharing knowledge.
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();

    let proj_one = store
        .writer
        .get_or_create_project(ws, name_one, None)
        .await
        .unwrap();
    let proj_two = store
        .writer
        .get_or_create_project(ws, name_two, None)
        .await
        .unwrap();
    assert_eq!(
        proj_one, proj_two,
        "both machines must land in the same project row"
    );

    store
        .writer
        .upsert_page(page(
            ws,
            proj_one,
            "notes/from-machine-one.md",
            "From machine one",
            "written on the laptop",
        ))
        .await
        .unwrap();

    let seen_from_machine_two = store
        .reader
        .page_body_by_ids(ws, proj_two, "notes/from-machine-one.md")
        .await
        .unwrap()
        .expect("machine two must see machine one's page");
    assert!(seen_from_machine_two.body.contains("written on the laptop"));
}
