//! SQLite storage layer for ai-memory.
//!
//! The crate owns a single SQLite file under `<data_dir>/db/memory.sqlite`,
//! opens it in WAL mode with foreign keys on, runs all pending migrations
//! at startup, and exposes a [`WriterHandle`] that serialises every mutation
//! through a dedicated OS thread.
//!
//! Reader-side APIs land in milestone M1-B; the writer + migrations are
//! sufficient for M1-A's "drop a page in, see it persisted" demo.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

pub mod decay;
mod error;
mod fts_query;
mod migrations;
mod ops;
mod reader;
mod writer;

pub use fts_query::prepare_fts5_query;

pub use decay::{DecayParams, retention_score};
pub use error::{StoreError, StoreResult};
pub use ops::{EmbeddingWrite, PurgeSummary, ReorgSummary};
pub use reader::{
    ActivityWindow, BriefingPage, BriefingSnapshot, DecayCandidate, DerivedIndexStatus,
    EmbeddingTripleCount, HealthDetail, HealthPage, ObservationHit, PageHit, PageHitWithMeta,
    PageLinks, PageMeta, PageSummary, ProjectSummary, ReaderPool, RelatedPage, StatusCounts,
    StoredEmbedding, WorkspaceSummary, f32_vec_to_bytes,
};
pub use writer::WriterHandle;

/// Filename used inside the data dir's `db/` subdirectory.
pub const DB_FILENAME: &str = "memory.sqlite";

/// Default soft cap for the read-only connection pool.
const READER_POOL_SOFT_CAP: usize = 4;

/// Open (and migrate) a [`Store`] rooted at the given data directory.
pub struct Store {
    /// Cloneable handle to submit mutations.
    pub writer: WriterHandle,
    /// Cloneable handle for read-only queries.
    pub reader: ReaderPool,
    db_path: PathBuf,
}

impl Store {
    /// Open the SQLite file at `<data_dir>/db/memory.sqlite`, applying any
    /// outstanding migrations, then spawn the writer thread and prepare
    /// the read-only connection pool.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the file cannot be opened, migrations
    /// cannot be applied, or the writer thread fails to start.
    pub fn open(data_dir: &Path) -> StoreResult<Self> {
        let db_dir = data_dir.join("db");
        std::fs::create_dir_all(&db_dir)?;
        let db_path = db_dir.join(DB_FILENAME);

        let mut conn = Connection::open(&db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?; // ms

        // SQLite cannot disable FK enforcement inside refinery's per-migration
        // transaction. Keep it off while migrations rebuild tables, then enable
        // it for all runtime reads/writes below.
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        migrations::run(&mut conn)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let writer = WriterHandle::spawn(conn);
        let reader = ReaderPool::new(&db_path, READER_POOL_SOFT_CAP)?;
        Ok(Self {
            writer,
            reader,
            db_path,
        })
    }

    /// Path of the SQLite file on disk.
    #[must_use]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{
        AgentKind, NewObservation, NewPage, NewSession, ObservationId, ObservationKind, PagePath,
        ProjectId, SessionId, Tier, WorkspaceId,
    };
    use rusqlite::{Connection, params};
    use tempfile::TempDir;

    fn sample_page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str) -> NewPage {
        NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path).unwrap(),
            title: "test".into(),
            body: body.into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
        }
    }

    #[tokio::test]
    async fn open_and_upsert_page() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();
        let id_a = store
            .writer
            .upsert_page(sample_page(ws, proj, "foo.md", "hello"))
            .await
            .unwrap();
        // Same body again: returns the same id, no supersession.
        let id_b = store
            .writer
            .upsert_page(sample_page(ws, proj, "foo.md", "hello"))
            .await
            .unwrap();
        assert_eq!(id_a, id_b);
        // Different body: supersession produces a new id.
        let id_c = store
            .writer
            .upsert_page(sample_page(ws, proj, "foo.md", "hello world"))
            .await
            .unwrap();
        assert_ne!(id_b, id_c);
    }

    #[tokio::test]
    async fn get_or_create_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let a = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let b = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        assert_eq!(a, b);
        let pa = store
            .writer
            .get_or_create_project(a, "scratch", None)
            .await
            .unwrap();
        let pb = store
            .writer
            .get_or_create_project(a, "scratch", None)
            .await
            .unwrap();
        assert_eq!(pa, pb);
    }

    #[tokio::test]
    async fn session_agent_kind_migrations_preserve_observations() {
        let tmp = TempDir::new().unwrap();
        let db_dir = tmp.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join(DB_FILENAME);
        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        crate::migrations::run_to(&mut conn, 8).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        let sid = SessionId::new();
        let oid = ObservationId::new();
        conn.execute(
            "INSERT INTO workspaces (id, name, created_at) VALUES (?1, 'default', 1)",
            params![ws.as_bytes()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO projects (id, workspace_id, name, created_at) \
             VALUES (?1, ?2, 'scratch', 1)",
            params![proj.as_bytes(), ws.as_bytes()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, started_at) \
             VALUES (?1, ?2, ?3, 'open-code', 1)",
            params![sid.as_bytes(), ws.as_bytes(), proj.as_bytes()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO observations (id, session_id, workspace_id, project_id, kind, title, body, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'user_prompt', 'keep', 'this observation must survive', 1)",
            params![oid.as_bytes(), sid.as_bytes(), ws.as_bytes(), proj.as_bytes()],
        )
        .unwrap();
        drop(conn);

        let store = Store::open(tmp.path()).unwrap();
        let count = store.reader.status_counts().await.unwrap().observations;
        assert_eq!(count, 1, "V09 must not cascade-delete observations");

        store
            .writer
            .begin_session(NewSession {
                id: SessionId::new(),
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::AntigravityCli,
                cwd: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn serialises_parallel_writes() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();
        // Spawn 16 concurrent writes; the writer must serialise them.
        let mut handles = Vec::new();
        for i in 0..16 {
            let w = store.writer.clone();
            handles.push(tokio::spawn(async move {
                w.upsert_page(sample_page(
                    ws,
                    proj,
                    &format!("p{i}.md"),
                    &format!("body-{i}"),
                ))
                .await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn recent_pages_returns_latest_only_in_order() {
        let tmp = TempDir::new().unwrap();
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
        for i in 0..3u8 {
            store
                .writer
                .upsert_page(sample_page(
                    ws,
                    proj,
                    &format!("p{i}.md"),
                    &format!("body-{i}"),
                ))
                .await
                .unwrap();
        }
        // Bump the second page to force a later updated_at.
        store
            .writer
            .upsert_page(sample_page(ws, proj, "p1.md", "body-1-rev"))
            .await
            .unwrap();

        let hits = store.reader.recent_pages(10).await.unwrap();
        assert_eq!(hits.len(), 3, "is_latest filter should give us 3 pages");
        assert_eq!(
            hits[0].path.as_str(),
            "p1.md",
            "most-recently-updated first"
        );
    }

    #[tokio::test]
    async fn status_counts_zero_on_fresh_db() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let counts = store.reader.status_counts().await.unwrap();
        assert_eq!(counts.pages_latest, 0);
        assert_eq!(counts.pages_all, 0);
        assert_eq!(counts.sessions, 0);
        assert_eq!(counts.observations, 0);
    }

    #[tokio::test]
    async fn search_finds_inserted_page_and_counts_reflect_supersession() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();

        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "alpha.md",
                "the quick brown fox jumps over the lazy dog",
            ))
            .await
            .unwrap();

        let hits = store.reader.search_pages("quick".into(), 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path.as_str(), "alpha.md");
        assert!(hits[0].snippet.contains("<mark>quick</mark>"));

        // Supersede: only the latest version should appear in counts'
        // pages_latest, and search should still return exactly one hit.
        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "alpha.md",
                "a different sentence with quick still inside",
            ))
            .await
            .unwrap();

        let counts = store.reader.status_counts().await.unwrap();
        assert_eq!(counts.pages_latest, 1);
        assert_eq!(counts.pages_all, 2);

        let hits = store.reader.search_pages("quick".into(), 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.contains("different"),
            "snippet should come from the latest version, got: {}",
            hits[0].snippet
        );
    }

    /// Regression: bare `word:` in agent queries is FTS5 column syntax, not
    /// a literal token (`no such column: pick` / `memory`).
    #[tokio::test]
    async fn search_colon_tokens_do_not_error() {
        let tmp = TempDir::new().unwrap();
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
            .upsert_page(sample_page(
                ws,
                proj,
                "handoff.md",
                "pick up handoff context from ai-memory bootstrap",
            ))
            .await
            .unwrap();

        let hits = store
            .reader
            .search_pages("pick: handoff bootstrap".into(), 10)
            .await
            .unwrap();
        assert!(
            !hits.is_empty(),
            "colon-sanitized query should match without SQLite column error"
        );
    }

    #[tokio::test]
    async fn search_boolean_or_still_works() {
        let tmp = TempDir::new().unwrap();
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
            .upsert_page(sample_page(ws, proj, "quick.md", "quick answer"))
            .await
            .unwrap();

        let hits = store
            .reader
            .search_pages("quick OR slow".into(), 10)
            .await
            .unwrap();
        assert!(!hits.is_empty(), "OR must remain an FTS5 operator");
    }

    #[tokio::test]
    async fn search_quotes_hyphenated_tokens_for_fts5() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();

        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "hyphen.md",
                "the ai-memory token should be searchable",
            ))
            .await
            .unwrap();

        let hits = store
            .reader
            .search_pages_for_project(ws, proj, "ai-memory".into(), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path.as_str(), "hyphen.md");
    }

    #[tokio::test]
    async fn hybrid_search_includes_linked_neighbors() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();

        store
            .writer
            .upsert_page(sample_page(ws, proj, "target.md", "neighbor-only content"))
            .await
            .unwrap();
        let mut source = sample_page(ws, proj, "source.md", "needle source content");
        source.links = vec![PagePath::new("target.md").unwrap()];
        store.writer.upsert_page(source).await.unwrap();

        let hits = store
            .reader
            .hybrid_search(
                ws,
                proj,
                "needle".into(),
                None,
                String::new(),
                String::new(),
                0,
                10,
            )
            .await
            .unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.contains(&"source.md"));
        assert!(
            paths.contains(&"target.md"),
            "linked neighbor should be included"
        );
    }

    #[tokio::test]
    async fn observation_fts_finds_raw_fallback_hits() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory", None)
            .await
            .unwrap();
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(NewObservation {
                session_id,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: "the raw-only zebra detail lives here".into(),
                importance: 5,
            })
            .await
            .unwrap();

        let hits = store
            .reader
            .search_observations_for_project(ws, proj, "zebra".into(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, session_id);
        assert!(hits[0].snippet.contains("<mark>zebra</mark>"));
    }

    #[tokio::test]
    async fn list_projects_with_stats_returns_aggregates() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "my-project", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "a.md", "alpha"))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "b.md", "beta"))
            .await
            .unwrap();

        let summaries = store.reader.list_projects_with_stats().await.unwrap();
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(s.workspace_name, "default");
        assert_eq!(s.project_name, "my-project");
        assert_eq!(s.page_count, 2);
        assert!(s.last_updated.is_some());
    }

    #[tokio::test]
    async fn list_pages_returns_latest_pages_for_project() {
        let tmp = TempDir::new().unwrap();
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
            .upsert_page(sample_page(ws, proj, "x.md", "body x"))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "y.md", "body y"))
            .await
            .unwrap();
        // Supersede x.md — should still appear once (latest version).
        store
            .writer
            .upsert_page(sample_page(ws, proj, "x.md", "body x updated"))
            .await
            .unwrap();

        let pages = store.reader.list_pages("default", "scratch").await.unwrap();
        assert_eq!(pages.len(), 2, "only is_latest=1 pages");
        let paths: Vec<&str> = pages.iter().map(|p| p.path.as_str()).collect();
        assert!(paths.contains(&"x.md"));
        assert!(paths.contains(&"y.md"));
    }

    #[tokio::test]
    async fn page_meta_returns_metadata_for_existing_page() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "meta-test", None)
            .await
            .unwrap();
        let page = NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("decisions/d1.md").unwrap(),
            title: "Decision One".into(),
            body: "content here".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({"kind": "decision"}),
            pinned: true,
            links: Vec::new(),
        };
        store.writer.upsert_page(page).await.unwrap();

        let meta = store
            .reader
            .page_meta("default", "meta-test", "decisions/d1.md")
            .await
            .unwrap();
        let meta = meta.expect("page_meta should return Some for existing page");
        assert_eq!(meta.workspace_name, "default");
        assert_eq!(meta.project_name, "meta-test");
        assert_eq!(meta.path, "decisions/d1.md");
        assert_eq!(meta.title, "Decision One");
        assert_eq!(meta.kind, "decision");
        assert!(meta.pinned);

        // Non-existent page returns None.
        let none = store
            .reader
            .page_meta("default", "meta-test", "no-such.md")
            .await
            .unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn delete_stale_page_embeddings_removes_mismatched_rows() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "test", None)
            .await
            .unwrap();
        let p1 = store
            .writer
            .upsert_page(sample_page(ws, proj, "a.md", "body a"))
            .await
            .unwrap();
        let p2 = store
            .writer
            .upsert_page(sample_page(ws, proj, "b.md", "body b"))
            .await
            .unwrap();
        store
            .writer
            .store_embedding(
                p1,
                vec![0u8; 4],
                "google".into(),
                "models/gemini-embedding-001".into(),
                768,
            )
            .await
            .unwrap();
        store
            .writer
            .store_embedding(
                p2,
                vec![1u8; 4],
                "openai".into(),
                "openai/text-embedding-3-small".into(),
                1536,
            )
            .await
            .unwrap();
        let n = store
            .writer
            .delete_stale_page_embeddings(
                ws,
                Some(proj),
                "openai".into(),
                "openai/text-embedding-3-small".into(),
                1536,
            )
            .await
            .unwrap();
        assert_eq!(n, 1);
        let mismatch = store
            .reader
            .embedding_meta_for_mismatch(
                "openai".into(),
                "openai/text-embedding-3-small".into(),
                1536,
            )
            .await
            .unwrap();
        assert!(mismatch.is_empty());
    }
}
