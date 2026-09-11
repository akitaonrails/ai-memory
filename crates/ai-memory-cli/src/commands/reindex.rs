//! `ai-memory reindex` — rebuild the SQLite index from the wiki/ markdown.
//!
//! Walks the wiki tree, recreates workspaces/projects from each scope's
//! self-describing `_meta.md` manifest (preserving the ids the tree is keyed
//! by), and reindexes every page. This is the "DB is rebuildable from files"
//! guarantee made operable — e.g. to move a data dir onto a clean migration
//! lineage: drop the old `db/`, then `reindex` rebuilds it from the markdown.
//!
//! # Exception to invariant §16
//!
//! Like `reset`/`restore`, `reindex` is a documented exception to the
//! thin-HTTP-client rule: it opens the store directly and must run with the
//! server stopped (a live SQLite writer would race the rebuild). The sysinfo
//! guard enforces this.
//!
//! Episodic DB-only state (sessions, observations, handoffs, decay counters)
//! is NOT in the markdown and is not reconstructed; embeddings can be
//! recomputed afterwards via `ai-memory embed`.

use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use anyhow::{Context, Result, bail};
use tracing::info;

use crate::cli::ReindexArgs;
use crate::config::Config;
use crate::process_guard::{busy_message, sibling_processes};

/// Run the `reindex` subcommand.
///
/// # Errors
/// Returns an error if another `ai-memory` process is running, the store
/// fails to open (e.g. a divergent migration lineage — rebuild onto a fresh
/// `db/` instead), or a scope directory lacks its `_meta.md` manifest.
pub async fn run(config: &Config, _args: ReindexArgs) -> Result<()> {
    let siblings = sibling_processes();
    if !siblings.is_empty() {
        bail!(busy_message("reindex", &siblings));
    }

    let store = Store::open(&config.data_dir).context("opening store for reindex")?;
    let target = store
        .reader
        .reindex_target_status()
        .await
        .context("checking whether the SQLite store is clean for reindex")?;
    if !target.is_clean() {
        bail!(
            "refusing to reindex into a non-empty SQLite store ({counts}). \
             `ai-memory reindex` rebuilds a clean derived DB from wiki files; \
             it is not an in-place dirty-index repair. Stop the server, take a \
             backup, move or remove {db_path}, then re-run reindex. DB-only \
             state (sessions, observations, handoffs, users, audit rows, \
             embeddings, decay counters) is not reconstructed from markdown.",
            counts = target.nonzero_summary(),
            db_path = store.db_path().display(),
        );
    }
    let wiki = Wiki::new(&config.data_dir, store.writer.clone())
        .context("opening wiki")?
        // Reindex is index-rebuild only; embeddings are recomputed separately
        // via `embed` so a missing/paid provider never blocks a rebuild.
        .without_embedder();

    let summary = wiki
        .reindex_all()
        .await
        .context("rebuilding index from wiki/")?;
    info!(
        workspaces = summary.workspaces,
        projects = summary.projects,
        pages = summary.pages,
        "reindex complete"
    );
    println!(
        "reindexed {} pages across {} project(s) in {} workspace(s) from {}",
        summary.pages,
        summary.projects,
        summary.workspaces,
        config.data_dir.join("wiki").display(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{NewPage, PagePath, Tier};
    use tempfile::TempDir;

    #[tokio::test]
    async fn reindex_refuses_dirty_store() {
        let tmp = TempDir::new().unwrap();
        let config = Config {
            data_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };

        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: ws,
                project_id: proj,
                path: PagePath::new("notes/stale.md").unwrap(),
                title: "stale".into(),
                body: "stale body".into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
                expires_at: None,
                entities: Vec::new(),
                evidence: Vec::new(),
            })
            .await
            .unwrap();
        drop(store);

        let err = run(&config, ReindexArgs {}).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("refusing to reindex into a non-empty SQLite store"),
            "dirty DB must be rejected, got {err:#}"
        );
    }
}
