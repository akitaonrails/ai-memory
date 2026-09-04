//! Durable per-chunk progress for a resumable bootstrap.
//!
//! Unlike [`crate::session_consolidation`], this table carries no lease or
//! attempt state: `/admin/bootstrap` already serialises runs behind a lock, so
//! there is never a second worker to arbitrate against. What survives here is
//! only the expensive part — each completed chunk's pages — so a crash, a
//! restart, or a chunk that fails past its retry budget does not discard the
//! LLM calls already paid for.

use ai_memory_core::{ProjectId, WorkspaceId};
use jiff::Timestamp;
use rusqlite::{Connection, params};

use crate::error::StoreResult;

/// One completed chunk's recorded output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootstrapChunkProgress {
    /// One-based position of the chunk in the plan.
    pub chunk_index: u32,
    /// The chunk's pages, serialised as the JSON array the caller stored.
    pub pages_json: String,
    /// The model's stated reasoning for that chunk.
    pub rationale: String,
}

/// Record one chunk's output, replacing any earlier row for the same position.
///
/// Replacing rather than ignoring matters when a resumed run re-processes a
/// chunk it could not confirm: the newer output is the one the in-memory
/// accumulator will hold, so the stored row has to agree with it.
pub fn record_chunk(
    conn: &mut Connection,
    fingerprint: &str,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    chunk_index: u32,
    pages_json: &str,
    rationale: &str,
) -> StoreResult<()> {
    conn.execute(
        "INSERT OR REPLACE INTO bootstrap_chunk_progress \
         (fingerprint, workspace_id, project_id, chunk_index, pages_json, rationale, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            fingerprint,
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            i64::from(chunk_index),
            pages_json,
            rationale,
            Timestamp::now().as_microsecond(),
        ],
    )?;
    Ok(())
}

/// Load every recorded chunk for one fingerprint, in plan order.
///
/// Scoping the read to the fingerprint is what makes a positional
/// `chunk_index` safe to act on: a different pruned source set or chunk budget
/// hashes differently, so its rows are never returned here and the run starts
/// over instead of skipping work that no longer corresponds.
pub fn load_progress(
    conn: &Connection,
    fingerprint: &str,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<Vec<BootstrapChunkProgress>> {
    let mut stmt = conn.prepare(
        "SELECT chunk_index, pages_json, rationale \
         FROM bootstrap_chunk_progress \
         WHERE fingerprint = ?1 AND workspace_id = ?2 AND project_id = ?3 \
         ORDER BY chunk_index",
    )?;
    let rows = stmt.query_map(
        params![fingerprint, workspace_id.as_bytes(), project_id.as_bytes()],
        |row| {
            Ok(BootstrapChunkProgress {
                chunk_index: u32::try_from(row.get::<_, i64>(0)?).unwrap_or(u32::MAX),
                pages_json: row.get(1)?,
                rationale: row.get(2)?,
            })
        },
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Drop one project's recorded progress once the run has written its pages.
///
/// Clearing by project rather than by fingerprint also collects the rows of
/// earlier abandoned attempts whose inputs have since changed, which would
/// otherwise only be reachable by the age sweep.
pub fn clear_progress(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<usize> {
    let removed = conn.execute(
        "DELETE FROM bootstrap_chunk_progress WHERE workspace_id = ?1 AND project_id = ?2",
        params![workspace_id.as_bytes(), project_id.as_bytes()],
    )?;
    Ok(removed)
}

/// Delete progress rows older than `older_than`, whatever their fingerprint.
///
/// A run abandoned before its final write leaves rows no later run will match
/// once the sources move on; without this sweep they would sit in the store
/// until the project itself is deleted.
pub fn prune_stale(conn: &mut Connection, older_than: i64) -> StoreResult<usize> {
    let removed = conn.execute(
        "DELETE FROM bootstrap_chunk_progress WHERE created_at < ?1",
        params![older_than],
    )?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    async fn scoped_store() -> (tempfile::TempDir, Store, WorkspaceId, ProjectId) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project_id = store
            .writer
            .get_or_create_project(workspace_id, "project", None)
            .await
            .unwrap();
        (tmp, store, workspace_id, project_id)
    }

    #[tokio::test]
    async fn records_and_reloads_chunks_in_plan_order() {
        let (_tmp, store, ws, proj) = scoped_store().await;
        // Written out of order on purpose: resume seeds pages by chunk
        // position, so the read has to impose the order, not the write.
        for idx in [3_u32, 1, 2] {
            store
                .writer
                .record_bootstrap_chunk(
                    "fp".into(),
                    ws,
                    proj,
                    idx,
                    format!("[{{\"n\":{idx}}}]"),
                    format!("chunk {idx}"),
                )
                .await
                .unwrap();
        }

        let rows = store
            .reader
            .with_conn(move |conn| load_progress(conn, "fp", ws, proj))
            .await
            .unwrap();
        let indices: Vec<u32> = rows.iter().map(|r| r.chunk_index).collect();
        assert_eq!(indices, vec![1, 2, 3]);
        assert_eq!(rows[0].pages_json, "[{\"n\":1}]");
        assert_eq!(rows[2].rationale, "chunk 3");
    }

    #[tokio::test]
    async fn a_different_fingerprint_is_not_visible() {
        // The safety property: progress recorded for one source set must not
        // be handed to a run whose sources have changed.
        let (_tmp, store, ws, proj) = scoped_store().await;
        store
            .writer
            .record_bootstrap_chunk("old".into(), ws, proj, 1, "[]".into(), "r".into())
            .await
            .unwrap();

        let rows = store
            .reader
            .with_conn(move |conn| load_progress(conn, "new", ws, proj))
            .await
            .unwrap();
        assert!(
            rows.is_empty(),
            "a run whose inputs changed must start over, not resume"
        );
    }

    #[tokio::test]
    async fn re_recording_a_chunk_replaces_it() {
        let (_tmp, store, ws, proj) = scoped_store().await;
        for rationale in ["first", "second"] {
            store
                .writer
                .record_bootstrap_chunk("fp".into(), ws, proj, 1, "[]".into(), rationale.into())
                .await
                .unwrap();
        }

        let rows = store
            .reader
            .with_conn(move |conn| load_progress(conn, "fp", ws, proj))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "one row per (fingerprint, chunk)");
        assert_eq!(rows[0].rationale, "second");
    }

    #[tokio::test]
    async fn clearing_drops_every_fingerprint_for_the_project() {
        let (_tmp, store, ws, proj) = scoped_store().await;
        for fp in ["abandoned", "current"] {
            store
                .writer
                .record_bootstrap_chunk(fp.into(), ws, proj, 1, "[]".into(), "r".into())
                .await
                .unwrap();
        }

        let removed = store
            .writer
            .clear_bootstrap_progress(ws, proj)
            .await
            .unwrap();
        assert_eq!(
            removed, 2,
            "a completed run also collects earlier abandoned attempts"
        );
        let rows = store
            .reader
            .with_conn(move |conn| load_progress(conn, "current", ws, proj))
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn pruning_removes_only_rows_older_than_the_cutoff() {
        let (_tmp, store, ws, proj) = scoped_store().await;
        store
            .writer
            .record_bootstrap_chunk("fp".into(), ws, proj, 1, "[]".into(), "r".into())
            .await
            .unwrap();

        // Anchor both cutoffs on the row's own timestamp rather than on a
        // second clock reading: `record_bootstrap_chunk` stamps the row inside
        // the writer, and a cutoff derived from a later `now()` can land in the
        // same microsecond.
        let written_at = store
            .reader
            .with_conn(|conn| {
                let ts: i64 = conn.query_row(
                    "SELECT created_at FROM bootstrap_chunk_progress",
                    [],
                    |row| row.get(0),
                )?;
                Ok(ts)
            })
            .await
            .unwrap();

        assert_eq!(
            store
                .writer
                .prune_bootstrap_progress(written_at)
                .await
                .unwrap(),
            0,
            "the cutoff is exclusive: a row stamped exactly at it must survive"
        );
        assert_eq!(
            store
                .writer
                .prune_bootstrap_progress(written_at + 1)
                .await
                .unwrap(),
            1,
        );
    }
}
