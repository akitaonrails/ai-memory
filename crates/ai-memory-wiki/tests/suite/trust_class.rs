//! Page trust class across a rebuild from the markdown (#2164, #2392).
//!
//! `pages.trust_class` lives only in SQLite, so `Wiki::reindex_all` cannot
//! recover it when it rebuilds a clean store from the tree. What these tests
//! pin is that a rebuild does not hand every page the most trusted class,
//! while a file the watcher reindexes after a hand edit still lands as
//! operator-authored.

use std::path::Path;

use ai_memory_core::{ActorContext, PagePath, ProjectId, Tier, TrustClass, WorkspaceId};
use ai_memory_store::Store;
use ai_memory_wiki::{Wiki, WritePageRequest};
use tempfile::TempDir;

fn write_request(
    ws: WorkspaceId,
    proj: ProjectId,
    path: &str,
    trust_class: TrustClass,
) -> WritePageRequest {
    WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        frontmatter: serde_json::Value::Null,
        body: format!("Text of {path}.\n"),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ActorContext::anonymous(),
        evidence: Vec::new(),
        trust_class: Some(trust_class),
    }
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

async fn class_of(store: &Store, ws: WorkspaceId, proj: ProjectId, path: &str) -> TrustClass {
    store
        .reader
        .page_trust_class_by_path(ws, proj, path)
        .await
        .unwrap()
        .expect("the path has a latest version")
}

#[tokio::test]
async fn reindex_rebuild_does_not_mark_rebuilt_pages_operator_authored() {
    let src = TempDir::new().unwrap();
    let source = Store::open(src.path()).unwrap();
    let ws = source
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = source
        .writer
        .get_or_create_project(ws, "trust", None)
        .await
        .unwrap();
    let wiki = Wiki::new(src.path(), source.writer.clone())
        .unwrap()
        .with_store_reader(source.reader.clone());
    for (path, class) in [
        ("notes/fetched.md", TrustClass::ToolResult),
        ("notes/typed.md", TrustClass::OperatorAuthored),
    ] {
        wiki.write_page(write_request(ws, proj, path, class))
            .await
            .unwrap();
        assert_eq!(class_of(&source, ws, proj, path).await, class);
    }
    drop(wiki);
    drop(source);

    // `ai-memory reindex`: a clean store rebuilt from the wiki tree alone.
    let dst = TempDir::new().unwrap();
    copy_tree(&src.path().join("wiki"), &dst.path().join("wiki"));
    let rebuilt = Store::open(dst.path()).unwrap();
    let rebuilt_wiki = Wiki::new(dst.path(), rebuilt.writer.clone()).unwrap();
    let summary = rebuilt_wiki.reindex_all().await.unwrap();
    assert_eq!(summary.pages, 2);

    // Neither class is in the markdown, so both pages take the class of a row
    // whose origin was never recorded. The page compiled from a tool result
    // must not come back as text the operator wrote.
    for path in ["notes/fetched.md", "notes/typed.md"] {
        assert_eq!(
            class_of(&rebuilt, ws, proj, path).await,
            TrustClass::ModelGenerated,
            "{path} after a rebuild"
        );
    }

    // Control, the watcher's path: an unchanged file keeps the rebuilt class,
    // and a hand edit on disk lands as operator-authored.
    let fetched = PagePath::new("notes/fetched.md").unwrap();
    rebuilt_wiki
        .reindex_page(ws, proj, fetched.clone())
        .await
        .unwrap();
    assert_eq!(
        class_of(&rebuilt, ws, proj, fetched.as_str()).await,
        TrustClass::ModelGenerated
    );
    let file = rebuilt_wiki.abs_path(ws, proj, &fetched);
    let edited = std::fs::read_to_string(&file).unwrap() + "\nChecked by hand.\n";
    std::fs::write(&file, edited).unwrap();
    rebuilt_wiki.reindex_page(ws, proj, fetched).await.unwrap();
    assert_eq!(
        class_of(&rebuilt, ws, proj, "notes/fetched.md").await,
        TrustClass::OperatorAuthored
    );
}

#[tokio::test]
async fn decay_tombstone_resurrection_preserves_tool_result_trust_class() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "trust", None)
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());

    let path = PagePath::new("notes/fetched.md").unwrap();
    let page_id = wiki
        .write_page(write_request(
            ws,
            proj,
            path.as_str(),
            TrustClass::ToolResult,
        ))
        .await
        .unwrap();
    assert_eq!(
        class_of(&store, ws, proj, path.as_str()).await,
        TrustClass::ToolResult
    );

    let abs = wiki.abs_path(ws, proj, &path);
    let on_disk = std::fs::read(&abs).unwrap();
    assert!(
        wiki.evict_page_if_latest(ws, proj, &path, page_id, None)
            .await
            .unwrap()
    );
    // Simulate the markdown file still being present on disk when decay
    // cleanup runs, triggering resurrection via `reindex_page_locked`.
    std::fs::write(&abs, &on_disk).unwrap();

    wiki.hard_delete_decay_tombstone(ws, proj, &path, page_id, i64::MAX)
        .await
        .unwrap();

    assert_eq!(
        class_of(&store, ws, proj, path.as_str()).await,
        TrustClass::ToolResult,
        "automated decay-tombstone resurrection must preserve tombstone_trust \
         instead of laundering a tool_result page into operator_authored"
    );
}
