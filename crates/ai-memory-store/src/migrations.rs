//! refinery-driven schema migrations.

use crate::error::{StoreError, StoreResult};

refinery::embed_migrations!("migrations");

/// Checksum recorded by refinery for the V62 SQL shipped in v2.2.1.
const ORIGINAL_V62_CHECKSUM: u64 = 16_518_128_245_137_964_566;
const V62_VERSION: u32 = 62;
const V62_NAME: &str = "page_ingestion_windows";

/// Run all pending migrations against an open connection.
///
/// # Errors
/// Propagates the underlying refinery error if a migration fails. When the
/// store is *ahead* of this binary — an applied migration is absent from the
/// compiled-in set, which refinery reports as `MissingVersion` with the
/// misleading text "migration V… is missing from the filesystem" — the error
/// is remapped to [`StoreError::DataSchemaAhead`], which names the offending
/// migration and points the operator at the fix.
pub fn run(conn: &mut rusqlite::Connection) -> StoreResult<()> {
    repair_original_v62_checksum(conn)?;
    migrations::runner()
        .set_abort_divergent(true)
        .set_abort_missing(true)
        .run(conn)
        .map_err(classify_run_error)?;
    Ok(())
}

/// Rewrite only the known V62 checksum produced by v2.2.1. The replacement
/// SQL moved an equivalent backfill to the bounded boot path (#776); stores
/// that applied the original migration are already in the same end state.
/// Every other checksum mismatch remains a hard refinery error.
fn repair_original_v62_checksum(conn: &rusqlite::Connection) -> StoreResult<()> {
    let has_history: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master \
         WHERE type = 'table' AND name = 'refinery_schema_history')",
        [],
        |row| row.get(0),
    )?;
    if !has_history {
        return Ok(());
    }
    let Some(current_checksum) = migrations::runner()
        .get_migrations()
        .iter()
        .find(|migration| migration.version() == V62_VERSION)
        .map(refinery::Migration::checksum)
    else {
        return Ok(());
    };
    let repaired = conn.execute(
        "UPDATE refinery_schema_history SET checksum = ?1 \
         WHERE version = ?2 AND name = ?3 AND checksum = ?4",
        rusqlite::params![
            current_checksum.to_string(),
            V62_VERSION,
            V62_NAME,
            ORIGINAL_V62_CHECKSUM.to_string()
        ],
    )?;
    if repaired == 1 {
        tracing::info!(
            version = V62_VERSION,
            "repaired known V62 migration checksum from the v2.2.1 layout"
        );
    }
    Ok(())
}

/// Highest schema version baked into this binary (the max embedded migration).
fn max_supported_version() -> u32 {
    migrations::runner()
        .get_migrations()
        .iter()
        .map(refinery::Migration::version)
        .max()
        .unwrap_or(0)
}

/// Translate refinery's raw error into a store-domain error. The only variant
/// reshaped is `MissingVersion` (the store's schema is ahead of this binary);
/// every other refinery failure passes through as [`StoreError::Migration`].
fn classify_run_error(err: refinery::Error) -> StoreError {
    if let refinery::error::Kind::MissingVersion(applied) = err.kind() {
        return StoreError::DataSchemaAhead {
            applied: format!("V{} ({})", applied.version(), applied.name()),
            supported: max_supported_version(),
        };
    }
    StoreError::Migration(err)
}

#[cfg(test)]
pub(crate) fn run_to(conn: &mut rusqlite::Connection, target: u32) -> Result<(), refinery::Error> {
    migrations::runner()
        .set_target(refinery::Target::Version(target))
        .run(conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{
        AgentKind, HandoffId, NewObservation, NewSession, ObservationKind, SessionId,
    };
    use rusqlite::{Connection, params};

    /// A store migrated by a newer build (an applied version above anything
    /// this binary embeds) must fail to open with the actionable
    /// `DataSchemaAhead` error, not refinery's raw "missing from the
    /// filesystem" wording.
    #[test]
    fn data_ahead_of_binary_reports_schema_ahead_not_raw_refinery() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("memory.sqlite");
        let mut conn = Connection::open(&db_path).unwrap();

        // Bring the store up to this binary's current schema.
        run(&mut conn).unwrap();

        // Simulate data written by a *newer* build: forge an applied migration
        // whose version sits above the embedded ceiling. refinery stores
        // `applied_on` as RFC3339 and `checksum` as a u64 string, and parses
        // both eagerly, so the row must be well-formed.
        let future = max_supported_version() + 100;
        conn.execute(
            "INSERT INTO refinery_schema_history (version, name, applied_on, checksum) \
             VALUES (?1, ?2, ?3, ?4)",
            params![future, "future_feature", "2026-07-14T00:00:00Z", "0"],
        )
        .unwrap();

        let err = run(&mut conn).unwrap_err();
        match err {
            StoreError::DataSchemaAhead { applied, supported } => {
                assert!(applied.contains(&format!("V{future}")), "applied={applied}");
                assert!(applied.contains("future_feature"), "applied={applied}");
                assert_eq!(supported, max_supported_version());
            }
            other => panic!("expected DataSchemaAhead, got: {other:?}"),
        }
    }

    /// The rendered message must drop refinery's misleading phrasing and carry
    /// the operator-facing explanation and remedy.
    #[test]
    fn schema_ahead_message_is_actionable() {
        let rendered = StoreError::DataSchemaAhead {
            applied: "V99 (future_feature)".to_string(),
            supported: 30,
        }
        .to_string();

        assert!(
            !rendered.contains("missing from the filesystem"),
            "must not leak refinery's raw wording: {rendered}"
        );
        assert!(
            rendered.contains("newer than this ai-memory build"),
            "{rendered}"
        );
        assert!(rendered.contains("V99 (future_feature)"), "{rendered}");
        assert!(rendered.contains("through V30"), "{rendered}");
    }

    /// A migration that fails partway must surface as a typed
    /// `StoreError::Migration`, must not be recorded in refinery's history
    /// (per-migration transaction rollback), and re-running after the
    /// precondition is fixed must converge to the full embedded schema.
    #[test]
    fn failed_migration_rolls_back_and_recovers_after_fix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("memory.sqlite");
        let mut conn = Connection::open(&db_path).unwrap();

        // Migrate up to just before V31 (managed workstreams), then poison the
        // run by pre-creating a table V31 builds mid-script — after it has
        // already created `workstreams` and its pairing trigger.
        run_to(&mut conn, 30).unwrap();
        conn.execute("CREATE TABLE workstream_native_sessions (id INTEGER)", [])
            .unwrap();

        // (a) The failure surfaces as the typed migration error, not a panic
        // or a misclassified schema-ahead error.
        let err = run(&mut conn).unwrap_err();
        match &err {
            StoreError::Migration(_) => {}
            other => panic!("expected StoreError::Migration, got: {other:?}"),
        }

        // (c) No half-migrated state: refinery's history must stop at V30 —
        // the failed V31 is not recorded — and its earlier statements were
        // rolled back, leaving the poisoned table untouched.
        let applied = applied_versions(&conn);
        assert_eq!(
            applied.last(),
            Some(&30),
            "failed migration must not be recorded: {applied:?}"
        );
        assert!(
            !applied.contains(&31),
            "V31 must be absent from history: {applied:?}"
        );
        assert_eq!(schema_object_count(&conn, "table", "workstreams"), 0);
        assert_eq!(
            schema_object_count(&conn, "trigger", "workstreams_ws_proj_pairing_ai"),
            0,
            "statements before the failure must roll back with the migration"
        );
        let poisoned_cols: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('workstream_native_sessions')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            poisoned_cols, 1,
            "the pre-existing table must survive untouched"
        );

        // (b) Fix the precondition and re-run: the store converges to the
        // embedded ceiling with the real V31 schema in place.
        conn.execute("DROP TABLE workstream_native_sessions", [])
            .unwrap();
        run(&mut conn).unwrap();
        let applied = applied_versions(&conn);
        assert_eq!(
            applied.last(),
            Some(&i64::from(max_supported_version())),
            "re-run must reach the embedded ceiling: {applied:?}"
        );
        assert!(applied.contains(&31) && applied.contains(&32));
        assert_eq!(schema_object_count(&conn, "table", "workstreams"), 1);
        assert_eq!(
            schema_object_count(&conn, "trigger", "workstreams_ws_proj_pairing_ai"),
            1
        );
    }

    fn applied_versions(conn: &Connection) -> Vec<i64> {
        let mut stmt = conn
            .prepare("SELECT version FROM refinery_schema_history ORDER BY version")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<i64>, _>>()
            .unwrap()
    }

    fn schema_object_count(conn: &Connection, kind: &str, name: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
            params![kind, name],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn v28_to_v29_preserves_existing_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 28).unwrap();
        let workspace_id = [7_u8; 16];
        conn.execute(
            "INSERT INTO workspaces (id, name, created_at) VALUES (?1, 'existing', 1)",
            params![workspace_id.as_slice()],
        )
        .unwrap();

        run(&mut conn).unwrap();
        let name: String = conn
            .query_row(
                "SELECT name FROM workspaces WHERE id = ?1",
                params![workspace_id.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "existing");
        let state_table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'maintenance_scheduler_state'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state_table, 1);
    }

    #[test]
    fn v38_adds_scoped_entity_index_without_disturbing_existing_pages() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 37).unwrap();

        let ws1 = [1_u8; 16];
        let ws2 = [2_u8; 16];
        let proj1 = [3_u8; 16];
        let proj2 = [4_u8; 16];
        let page1 = [5_u8; 16];
        let page2 = [6_u8; 16];
        let hash = [0_u8; 32];
        for (workspace, name) in [(ws1, "one"), (ws2, "two")] {
            conn.execute(
                "INSERT INTO workspaces (id, name, created_at) VALUES (?1, ?2, 1)",
                params![workspace.as_slice(), name],
            )
            .unwrap();
        }
        for (project, workspace, name) in [(proj1, ws1, "project-one"), (proj2, ws2, "project-two")]
        {
            conn.execute(
                "INSERT INTO projects (id, workspace_id, name, created_at) \
                 VALUES (?1, ?2, ?3, 1)",
                params![project.as_slice(), workspace.as_slice(), name],
            )
            .unwrap();
        }
        for (page, workspace, project, path) in
            [(page1, ws1, proj1, "one.md"), (page2, ws2, proj2, "two.md")]
        {
            conn.execute(
                "INSERT INTO pages \
                 (id, workspace_id, project_id, path, title, tier, body, body_sha256, \
                  frontmatter_json, is_latest, pinned, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, 'title', 'semantic', 'body', ?5, '{}', 1, 0, 1, 1)",
                params![
                    page.as_slice(),
                    workspace.as_slice(),
                    project.as_slice(),
                    path,
                    hash.as_slice()
                ],
            )
            .unwrap();
        }

        run(&mut conn).unwrap();
        assert_eq!(schema_object_count(&conn, "table", "entities"), 1);
        assert_eq!(schema_object_count(&conn, "table", "entity_page_links"), 1);
        assert_eq!(
            schema_object_count(&conn, "trigger", "entities_ws_proj_pairing_ai"),
            1
        );
        assert_eq!(
            schema_object_count(&conn, "trigger", "entity_page_links_scope_pairing_ai"),
            1
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM pages", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2,
            "V38 must preserve existing pages"
        );

        let entity1 = [7_u8; 16];
        conn.execute(
            "INSERT INTO entities (id, workspace_id, project_id, name, created_at) \
             VALUES (?1, ?2, ?3, 'sqlite', 1)",
            params![entity1.as_slice(), ws1.as_slice(), proj1.as_slice()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entity_page_links (entity_id, page_id) VALUES (?1, ?2)",
            params![entity1.as_slice(), page1.as_slice()],
        )
        .unwrap();

        let wrong_scope = conn
            .execute(
                "INSERT INTO entities (id, workspace_id, project_id, name, created_at) \
                 VALUES (?1, ?2, ?3, 'mismatch', 1)",
                params![[8_u8; 16].as_slice(), ws2.as_slice(), proj1.as_slice()],
            )
            .unwrap_err();
        assert!(
            wrong_scope
                .to_string()
                .contains("workspace/project mismatch")
        );
        let wrong_page = conn
            .execute(
                "INSERT INTO entity_page_links (entity_id, page_id) VALUES (?1, ?2)",
                params![entity1.as_slice(), page2.as_slice()],
            )
            .unwrap_err();
        assert!(
            wrong_page
                .to_string()
                .contains("entity/page scope mismatch")
        );
        assert!(
            conn.execute(
                "INSERT INTO entities (id, workspace_id, project_id, name, created_at) \
                 VALUES (?1, ?2, ?3, ?4, 1)",
                params![
                    [9_u8; 16].as_slice(),
                    ws1.as_slice(),
                    proj1.as_slice(),
                    "x".repeat(65)
                ],
            )
            .is_err(),
            "the schema must enforce the public entity length bound"
        );
    }

    #[test]
    fn v33_to_v35_preserves_queue_and_backfills_end_generation() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 33).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace_id = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id =
            crate::ops::get_or_create_project(&mut conn, &workspace_id, "project", None).unwrap();
        let session_id = SessionId::new();
        // Era-appropriate raw insert. This fixture deliberately stops at V33
        // and then migrates forward, so it must not go through `begin_session`:
        // that writes whatever columns the CURRENT schema has, and every later
        // migration that adds one would break a test about an older era.
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at) \
             VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
            params![
                session_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                AgentKind::Codex.as_str(),
                jiff::Timestamp::now().as_microsecond(),
            ],
        )
        .unwrap();
        crate::ops::insert_observation(
            &mut conn,
            &NewObservation {
                occurred_at: None,
                session_id,
                workspace_id,
                project_id,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: "continue".into(),
                importance: 8,
            },
        )
        .unwrap();
        conn.execute(
            "UPDATE sessions SET ended_at = 1 WHERE id = ?1",
            params![session_id.as_bytes()],
        )
        .unwrap();

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        assert_eq!(schema_object_count(&conn, "table", "sessions"), 1);
        assert_eq!(
            schema_object_count(&conn, "table", "session_consolidation_jobs"),
            1
        );
        assert_eq!(
            schema_object_count(
                &conn,
                "trigger",
                "session_consolidation_jobs_session_pairing_ai"
            ),
            1
        );
        let ended_observation_count: u64 = conn
            .query_row(
                "SELECT ended_observation_count FROM sessions WHERE id = ?1",
                params![session_id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            ended_observation_count, 1,
            "V35 must baseline existing ended sessions without catch-up work"
        );
        assert!(
            crate::session_consolidation::enqueue(&mut conn, workspace_id, project_id, session_id,)
                .unwrap()
        );
    }

    #[test]
    fn v39_to_v41_preserve_existing_rows_as_shared_and_add_listing_indexes() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 38).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace_id = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id =
            crate::ops::get_or_create_project(&mut conn, &workspace_id, "project", None).unwrap();
        let session_id = SessionId::new();
        let handoff_id = HandoffId::new();
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at) \
             VALUES (?1, ?2, ?3, ?4, NULL, 1)",
            params![
                session_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                AgentKind::Codex.as_str(),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO handoffs \
             (id, workspace_id, project_id, from_agent, summary, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            params![
                handoff_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                AgentKind::Codex.as_str(),
                "legacy baton",
            ],
        )
        .unwrap();

        run(&mut conn).unwrap();

        let actor_user: Option<String> = conn
            .query_row(
                "SELECT actor_user FROM sessions WHERE id = ?1",
                params![session_id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        let (owner_user, accepted_by_user): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT owner_user, accepted_by_user FROM handoffs WHERE id = ?1",
                params![handoff_id.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(actor_user, None, "legacy sessions remain shared");
        assert_eq!(owner_user, None, "legacy handoffs remain shared");
        assert_eq!(accepted_by_user, None);
        for index in [
            "idx_handoffs_open_owner",
            "idx_sessions_open_owner",
            "idx_handoffs_project_recent",
            "idx_handoffs_project_owner_recent",
        ] {
            assert_eq!(
                schema_object_count(&conn, "index", index),
                1,
                "missing ownership/listing index {index}",
            );
        }
    }

    #[test]
    fn v43_to_v44_preserves_session_state_and_all_scope_guards() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 43).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace_id = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id =
            crate::ops::get_or_create_project(&mut conn, &workspace_id, "project", None).unwrap();
        let existing = SessionId::new();
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, ended_at, \
              ended_observation_count, actor_user) \
             VALUES (?1, ?2, ?3, 'codex', '/repo', 1, 2, 7, 'user:alice')",
            params![
                existing.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )
        .unwrap();

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let preserved: (String, String, i64, Option<String>) = conn
            .query_row(
                "SELECT agent_kind, cwd, ended_observation_count, actor_user \
                 FROM sessions WHERE id = ?1",
                params![existing.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            preserved,
            ("codex".into(), "/repo".into(), 7, Some("user:alice".into()))
        );

        for index in [
            "idx_sessions_recent",
            "idx_sessions_project",
            "idx_sessions_started_at",
            "idx_sessions_scope_ended",
            "idx_sessions_open_owner",
        ] {
            assert_eq!(
                schema_object_count(&conn, "index", index),
                1,
                "V44 dropped session index {index}"
            );
        }
        for trigger in [
            "sessions_ws_proj_pairing_ai",
            "auto_improve_scheduler_claims_session_pairing_ai",
            "session_consolidation_jobs_session_pairing_ai",
        ] {
            assert_eq!(
                schema_object_count(&conn, "trigger", trigger),
                1,
                "V44 dropped scope guard {trigger}"
            );
        }
        let foreign_key_errors: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(foreign_key_errors, 0);

        crate::ops::begin_session(
            &mut conn,
            &NewSession {
                occurred_at: None,
                id: SessionId::new(),
                workspace_id,
                project_id,
                agent_kind: AgentKind::Hermes,
                cwd: Some("/repo".into()),
                actor_user: Some("user:alice".into()),
            },
        )
        .unwrap();
    }

    #[test]
    fn v44_to_v45_preserves_session_state_and_accepts_kiro_cli() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 44).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace_id = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id =
            crate::ops::get_or_create_project(&mut conn, &workspace_id, "project", None).unwrap();
        let existing = SessionId::new();
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, ended_at, \
              ended_observation_count, actor_user) \
             VALUES (?1, ?2, ?3, 'hermes', '/repo', 1, 2, 7, 'user:alice')",
            params![
                existing.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )
        .unwrap();

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let preserved: (String, String, i64, Option<String>) = conn
            .query_row(
                "SELECT agent_kind, cwd, ended_observation_count, actor_user \
                 FROM sessions WHERE id = ?1",
                params![existing.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            preserved,
            (
                "hermes".into(),
                "/repo".into(),
                7,
                Some("user:alice".into())
            )
        );

        crate::ops::begin_session(
            &mut conn,
            &NewSession {
                occurred_at: None,
                id: SessionId::new(),
                workspace_id,
                project_id,
                agent_kind: AgentKind::KiroCli,
                cwd: Some("/repo".into()),
                actor_user: Some("user:alice".into()),
            },
        )
        .unwrap();

        for name in [
            "idx_sessions_open_owner",
            "sessions_ws_proj_pairing_ai",
            "auto_improve_scheduler_claims_session_pairing_ai",
            "session_consolidation_jobs_session_pairing_ai",
        ] {
            let kind = if name.starts_with("idx_") {
                "index"
            } else {
                "trigger"
            };
            assert_eq!(schema_object_count(&conn, kind, name), 1, "missing {name}");
        }
    }

    #[test]
    fn v46_to_v47_preserves_session_state_and_accepts_command_code() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 46).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace_id = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id =
            crate::ops::get_or_create_project(&mut conn, &workspace_id, "project", None).unwrap();
        let existing = SessionId::new();
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, ended_at, \
              ended_observation_count, actor_user) \
             VALUES (?1, ?2, ?3, 'kiro-cli', '/repo', 1, 2, 7, 'user:alice')",
            params![
                existing.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )
        .unwrap();

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let preserved: (String, String, i64, Option<String>) = conn
            .query_row(
                "SELECT agent_kind, cwd, ended_observation_count, actor_user \
                 FROM sessions WHERE id = ?1",
                params![existing.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            preserved,
            (
                "kiro-cli".into(),
                "/repo".into(),
                7,
                Some("user:alice".into())
            )
        );

        crate::ops::begin_session(
            &mut conn,
            &NewSession {
                occurred_at: None,
                id: SessionId::new(),
                workspace_id,
                project_id,
                agent_kind: AgentKind::CommandCode,
                cwd: Some("/repo".into()),
                actor_user: Some("user:alice".into()),
            },
        )
        .unwrap();

        for name in [
            "idx_sessions_open_owner",
            "sessions_ws_proj_pairing_ai",
            "auto_improve_scheduler_claims_session_pairing_ai",
            "session_consolidation_jobs_session_pairing_ai",
        ] {
            let kind = if name.starts_with("idx_") {
                "index"
            } else {
                "trigger"
            };
            assert_eq!(schema_object_count(&conn, kind, name), 1, "missing {name}");
        }
    }

    #[test]
    fn v49_to_v50_preserves_session_state_and_accepts_pool() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 49).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace_id = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id =
            crate::ops::get_or_create_project(&mut conn, &workspace_id, "project", None).unwrap();
        let existing = SessionId::new();
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, ended_at, \
              ended_observation_count, actor_user) \
             VALUES (?1, ?2, ?3, 'command-code', '/repo', 1, 2, 7, 'user:alice')",
            params![
                existing.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )
        .unwrap();

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let preserved: (String, String, i64, Option<String>) = conn
            .query_row(
                "SELECT agent_kind, cwd, ended_observation_count, actor_user \
                 FROM sessions WHERE id = ?1",
                params![existing.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            preserved,
            (
                "command-code".into(),
                "/repo".into(),
                7,
                Some("user:alice".into())
            )
        );

        crate::ops::begin_session(
            &mut conn,
            &NewSession {
                occurred_at: None,
                id: SessionId::new(),
                workspace_id,
                project_id,
                agent_kind: AgentKind::Pool,
                cwd: Some("/repo".into()),
                actor_user: Some("user:alice".into()),
            },
        )
        .unwrap();

        for name in [
            "idx_sessions_open_owner",
            "sessions_ws_proj_pairing_ai",
            "auto_improve_scheduler_claims_session_pairing_ai",
            "session_consolidation_jobs_session_pairing_ai",
        ] {
            let kind = if name.starts_with("idx_") {
                "index"
            } else {
                "trigger"
            };
            assert_eq!(schema_object_count(&conn, kind, name), 1, "missing {name}");
        }
    }

    #[test]
    fn v50_to_v51_preserves_session_state_and_accepts_zcode() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 50).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace_id = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id =
            crate::ops::get_or_create_project(&mut conn, &workspace_id, "project", None).unwrap();
        let existing = SessionId::new();
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, ended_at, \
              ended_observation_count, actor_user) \
             VALUES (?1, ?2, ?3, 'pool', '/repo', 1, 2, 7, 'user:alice')",
            params![
                existing.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )
        .unwrap();

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let preserved: (String, String, i64, Option<String>) = conn
            .query_row(
                "SELECT agent_kind, cwd, ended_observation_count, actor_user \
                 FROM sessions WHERE id = ?1",
                params![existing.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            preserved,
            ("pool".into(), "/repo".into(), 7, Some("user:alice".into()))
        );

        crate::ops::begin_session(
            &mut conn,
            &NewSession {
                occurred_at: None,
                id: SessionId::new(),
                workspace_id,
                project_id,
                agent_kind: AgentKind::Zcode,
                cwd: Some("/repo".into()),
                actor_user: Some("user:alice".into()),
            },
        )
        .unwrap();

        for name in [
            "idx_sessions_open_owner",
            "sessions_ws_proj_pairing_ai",
            "auto_improve_scheduler_claims_session_pairing_ai",
            "session_consolidation_jobs_session_pairing_ai",
        ] {
            let kind = if name.starts_with("idx_") {
                "index"
            } else {
                "trigger"
            };
            assert_eq!(schema_object_count(&conn, kind, name), 1, "missing {name}");
        }
    }

    #[test]
    fn v71_to_v72_preserves_session_state_and_accepts_grizzybot() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 71).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace_id = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id =
            crate::ops::get_or_create_project(&mut conn, &workspace_id, "project", None).unwrap();
        let existing = SessionId::new();
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, ended_at, \
              ended_observation_count, actor_user) \
             VALUES (?1, ?2, ?3, 'zcode', '/repo', 1, 2, 7, 'user:alice')",
            params![
                existing.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )
        .unwrap();
        // V64 mail points at sessions with ON DELETE SET NULL; the rebuild
        // must neither null the sender nor leave a dangling reference.
        let message_id = uuid::Uuid::now_v7();
        conn.execute(
            "INSERT INTO agent_messages \
             (id, from_workspace_id, from_project_id, from_agent, from_session_id, \
              to_workspace_id, to_project_id, body, created_at) \
             VALUES (?1, ?2, ?3, 'zcode', ?4, ?2, ?3, 'hello', 1)",
            params![
                message_id.as_bytes().as_slice(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                existing.as_bytes(),
            ],
        )
        .unwrap();

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let sender: Option<Vec<u8>> = conn
            .query_row(
                "SELECT from_session_id FROM agent_messages WHERE id = ?1",
                params![message_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sender.as_deref(), Some(existing.as_bytes().as_slice()));
        let dangling: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_check('agent_messages')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dangling, 0, "agent_messages must still reference sessions");

        let preserved: (String, String, i64, Option<String>) = conn
            .query_row(
                "SELECT agent_kind, cwd, ended_observation_count, actor_user \
                 FROM sessions WHERE id = ?1",
                params![existing.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            preserved,
            ("zcode".into(), "/repo".into(), 7, Some("user:alice".into()))
        );

        crate::ops::begin_session(
            &mut conn,
            &NewSession {
                occurred_at: None,
                id: SessionId::new(),
                workspace_id,
                project_id,
                agent_kind: AgentKind::Grizzybot,
                cwd: Some("/repo".into()),
                actor_user: Some("user:alice".into()),
            },
        )
        .unwrap();

        for name in [
            "idx_sessions_open_owner",
            "sessions_ws_proj_pairing_ai",
            "auto_improve_scheduler_claims_session_pairing_ai",
            "session_consolidation_jobs_session_pairing_ai",
        ] {
            let kind = if name.starts_with("idx_") {
                "index"
            } else {
                "trigger"
            };
            assert_eq!(schema_object_count(&conn, kind, name), 1, "missing {name}");
        }
    }

    #[test]
    fn v72_to_v73_preserves_grizzybot_sessions_and_accepts_copilot_cli() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 72).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace_id = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id =
            crate::ops::get_or_create_project(&mut conn, &workspace_id, "project", None).unwrap();
        let existing = SessionId::new();
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, ended_at, \
              ended_observation_count, actor_user) \
             VALUES (?1, ?2, ?3, 'grizzybot', '/repo', 1, 2, 7, 'user:alice')",
            params![
                existing.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )
        .unwrap();

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let preserved: (String, String, i64, Option<String>) = conn
            .query_row(
                "SELECT agent_kind, cwd, ended_observation_count, actor_user \
                 FROM sessions WHERE id = ?1",
                params![existing.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            preserved,
            (
                "grizzybot".into(),
                "/repo".into(),
                7,
                Some("user:alice".into())
            )
        );

        crate::ops::begin_session(
            &mut conn,
            &NewSession {
                occurred_at: None,
                id: SessionId::new(),
                workspace_id,
                project_id,
                agent_kind: AgentKind::CopilotCli,
                cwd: Some("/repo".into()),
                actor_user: Some("user:alice".into()),
            },
        )
        .unwrap();

        for name in [
            "idx_sessions_open_owner",
            "sessions_ws_proj_pairing_ai",
            "auto_improve_scheduler_claims_session_pairing_ai",
            "session_consolidation_jobs_session_pairing_ai",
        ] {
            let kind = if name.starts_with("idx_") {
                "index"
            } else {
                "trigger"
            };
            assert_eq!(schema_object_count(&conn, kind, name), 1, "missing {name}");
        }
    }

    #[test]
    fn v77_to_v78_preserves_sessions_and_their_ingest_keys_and_accepts_droid() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 77).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace_id = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id =
            crate::ops::get_or_create_project(&mut conn, &workspace_id, "project", None).unwrap();
        let existing = SessionId::new();
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, ended_at, \
              ended_observation_count, actor_user) \
             VALUES (?1, ?2, ?3, 'copilot-cli', '/repo', 1, 2, 7, 'user:alice')",
            params![
                existing.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )
        .unwrap();
        // V77 points ingest_keys at sessions; the table rebuild must leave
        // that reference intact.
        conn.execute(
            "INSERT INTO ingest_keys (project_id, key, seen_at, session_id, agent_kind) \
             VALUES (?1, 'k1', 1, ?2, 'copilot-cli')",
            params![project_id.as_bytes(), existing.as_bytes()],
        )
        .unwrap();

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let preserved: (String, String, i64, Option<String>) = conn
            .query_row(
                "SELECT agent_kind, cwd, ended_observation_count, actor_user \
                 FROM sessions WHERE id = ?1",
                params![existing.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            preserved,
            (
                "copilot-cli".into(),
                "/repo".into(),
                7,
                Some("user:alice".into())
            )
        );
        let dangling: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_check('ingest_keys')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dangling, 0, "ingest_keys must still reference sessions");

        crate::ops::begin_session(
            &mut conn,
            &NewSession {
                occurred_at: None,
                id: SessionId::new(),
                workspace_id,
                project_id,
                agent_kind: AgentKind::Droid,
                cwd: Some("/repo".into()),
                actor_user: Some("user:alice".into()),
            },
        )
        .unwrap();

        for name in [
            "idx_sessions_open_owner",
            "sessions_ws_proj_pairing_ai",
            "auto_improve_scheduler_claims_session_pairing_ai",
            "session_consolidation_jobs_session_pairing_ai",
        ] {
            let kind = if name.starts_with("idx_") {
                "index"
            } else {
                "trigger"
            };
            assert_eq!(schema_object_count(&conn, kind, name), 1, "missing {name}");
        }
    }

    /// V80 rebuilds `sessions` again, for Meta Muse Code: rows, the ingest
    /// keys and observations that reference them, and the scope-pairing
    /// triggers all survive, and a `muse` session is accepted afterwards.
    #[test]
    fn v79_to_v80_preserves_sessions_ingest_keys_and_observations_and_accepts_muse() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 79).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace_id = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id =
            crate::ops::get_or_create_project(&mut conn, &workspace_id, "project", None).unwrap();
        let existing = SessionId::new();
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, ended_at, \
              ended_observation_count, actor_user) \
             VALUES (?1, ?2, ?3, 'droid', '/repo', 1, 2, 7, 'user:alice')",
            params![
                existing.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )
        .unwrap();
        // V79 cannot store a Muse session yet.
        assert!(
            conn.execute(
                "INSERT INTO sessions \
                 (id, workspace_id, project_id, agent_kind, started_at) \
                 VALUES (?1, ?2, ?3, 'muse', 1)",
                params![
                    SessionId::new().as_bytes(),
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                ],
            )
            .is_err(),
            "the V79 CHECK must reject muse"
        );
        conn.execute(
            "INSERT INTO ingest_keys (project_id, key, seen_at, session_id, agent_kind) \
             VALUES (?1, 'k1', 1, ?2, 'droid')",
            params![project_id.as_bytes(), existing.as_bytes()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO observations \
             (id, session_id, workspace_id, project_id, kind, title, body, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'prompt', 'kept', 'kept body', 1)",
            params![
                uuid::Uuid::new_v4().as_bytes(),
                existing.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )
        .unwrap();

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let preserved: (String, String, i64, Option<String>) = conn
            .query_row(
                "SELECT agent_kind, cwd, ended_observation_count, actor_user \
                 FROM sessions WHERE id = ?1",
                params![existing.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            preserved,
            ("droid".into(), "/repo".into(), 7, Some("user:alice".into()))
        );
        let observations: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM observations WHERE session_id = ?1",
                params![existing.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(observations, 1, "the session's observations survive");
        for table in ["ingest_keys", "observations"] {
            let dangling: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM pragma_foreign_key_check('{table}')"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(dangling, 0, "{table} must still reference sessions");
        }

        crate::ops::begin_session(
            &mut conn,
            &NewSession {
                occurred_at: None,
                id: SessionId::new(),
                workspace_id,
                project_id,
                agent_kind: AgentKind::Muse,
                cwd: Some("/repo".into()),
                actor_user: Some("user:alice".into()),
            },
        )
        .unwrap();

        for name in [
            "idx_sessions_open_owner",
            "sessions_ws_proj_pairing_ai",
            "auto_improve_scheduler_claims_session_pairing_ai",
            "session_consolidation_jobs_session_pairing_ai",
        ] {
            let kind = if name.starts_with("idx_") {
                "index"
            } else {
                "trigger"
            };
            assert_eq!(schema_object_count(&conn, kind, name), 1, "missing {name}");
        }
    }

    /// V81 rebuilds `sessions` again, for CodeBuddy Code: rows, the ingest
    /// keys and observations that reference them, and the scope-pairing
    /// triggers all survive, and a `codebuddy` session is accepted afterwards.
    #[test]
    fn v80_to_v81_preserves_sessions_ingest_keys_and_observations_and_accepts_codebuddy() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 80).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace_id = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id =
            crate::ops::get_or_create_project(&mut conn, &workspace_id, "project", None).unwrap();
        let existing = SessionId::new();
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, ended_at, \
              ended_observation_count, actor_user) \
             VALUES (?1, ?2, ?3, 'muse', '/repo', 1, 2, 7, 'user:alice')",
            params![
                existing.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )
        .unwrap();
        // V80 cannot store a CodeBuddy session yet.
        assert!(
            conn.execute(
                "INSERT INTO sessions \
                 (id, workspace_id, project_id, agent_kind, started_at) \
                 VALUES (?1, ?2, ?3, 'codebuddy', 1)",
                params![
                    SessionId::new().as_bytes(),
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                ],
            )
            .is_err(),
            "the V80 CHECK must reject codebuddy"
        );
        conn.execute(
            "INSERT INTO ingest_keys (project_id, key, seen_at, session_id, agent_kind) \
             VALUES (?1, 'k1', 1, ?2, 'muse')",
            params![project_id.as_bytes(), existing.as_bytes()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO observations \
             (id, session_id, workspace_id, project_id, kind, title, body, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'prompt', 'kept', 'kept body', 1)",
            params![
                uuid::Uuid::new_v4().as_bytes(),
                existing.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )
        .unwrap();

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let preserved: (String, String, i64, Option<String>) = conn
            .query_row(
                "SELECT agent_kind, cwd, ended_observation_count, actor_user \
                 FROM sessions WHERE id = ?1",
                params![existing.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            preserved,
            ("muse".into(), "/repo".into(), 7, Some("user:alice".into()))
        );
        let observations: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM observations WHERE session_id = ?1",
                params![existing.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(observations, 1, "the session's observations survive");
        for table in ["ingest_keys", "observations"] {
            let dangling: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM pragma_foreign_key_check('{table}')"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(dangling, 0, "{table} must still reference sessions");
        }

        crate::ops::begin_session(
            &mut conn,
            &NewSession {
                occurred_at: None,
                id: SessionId::new(),
                workspace_id,
                project_id,
                agent_kind: AgentKind::CodeBuddy,
                cwd: Some("/repo".into()),
                actor_user: Some("user:alice".into()),
            },
        )
        .unwrap();

        for name in [
            "idx_sessions_open_owner",
            "sessions_ws_proj_pairing_ai",
            "auto_improve_scheduler_claims_session_pairing_ai",
            "session_consolidation_jobs_session_pairing_ai",
        ] {
            let kind = if name.starts_with("idx_") {
                "index"
            } else {
                "trigger"
            };
            assert_eq!(schema_object_count(&conn, kind, name), 1, "missing {name}");
        }
    }

    /// V83 expands the sessions enum for IBM Bob. Existing sessions and
    /// dependent observations survive the table rebuild, while the old V82
    /// constraint rejects the new value and the migrated schema accepts it.
    #[test]
    fn v82_to_v83_preserves_sessions_and_observations_and_accepts_ibm_bob() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 82).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace_id = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id =
            crate::ops::get_or_create_project(&mut conn, &workspace_id, "project", None).unwrap();
        let existing = SessionId::new();
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, actor_user) \
             VALUES (?1, ?2, ?3, 'codebuddy', '/repo', 1, 'user:alice')",
            params![
                existing.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO sessions \
                 (id, workspace_id, project_id, agent_kind, started_at) \
                 VALUES (?1, ?2, ?3, 'ibm-bob', 1)",
                params![
                    SessionId::new().as_bytes(),
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                ],
            )
            .is_err(),
            "the V82 CHECK must reject ibm-bob"
        );
        conn.execute(
            "INSERT INTO observations \
             (id, session_id, workspace_id, project_id, kind, title, body, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'prompt', 'kept', 'kept body', 1)",
            params![
                uuid::Uuid::new_v4().as_bytes(),
                existing.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
            ],
        )
        .unwrap();

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let preserved: (String, String, Option<String>) = conn
            .query_row(
                "SELECT agent_kind, cwd, actor_user FROM sessions WHERE id = ?1",
                params![existing.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            preserved,
            (
                "codebuddy".into(),
                "/repo".into(),
                Some("user:alice".into())
            )
        );
        let observations: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM observations WHERE session_id = ?1",
                params![existing.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(observations, 1, "the session's observations survive");

        crate::ops::begin_session(
            &mut conn,
            &NewSession {
                occurred_at: None,
                id: SessionId::new(),
                workspace_id,
                project_id,
                agent_kind: AgentKind::IbmBob,
                cwd: Some("/repo".into()),
                actor_user: Some("user:alice".into()),
            },
        )
        .unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            0
        );
        for name in [
            "idx_sessions_open_owner",
            "sessions_ws_proj_pairing_ai",
            "auto_improve_scheduler_claims_session_pairing_ai",
            "session_consolidation_jobs_session_pairing_ai",
        ] {
            let kind = if name.starts_with("idx_") {
                "index"
            } else {
                "trigger"
            };
            assert_eq!(schema_object_count(&conn, kind, name), 1, "missing {name}");
        }
    }

    #[test]
    fn v81_to_v82_preserves_managed_runs_and_adds_previous_session_identity() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 81).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project =
            crate::ops::get_or_create_project(&mut conn, &workspace, "project", None).unwrap();
        let workstream = ai_memory_core::WorkstreamId::new();
        let run_id = ai_memory_core::ManagedRunId::new();
        conn.execute(
            "INSERT INTO workstreams \
             (id, workspace_id, project_id, repo_fingerprint, worktree_fingerprint, name, cwd, \
              created_at, selected_at, updated_at) \
             VALUES (?1, ?2, ?3, 'repo', 'tree', 'default', '/repo', 1, 1, 1)",
            params![
                workstream.as_bytes(),
                workspace.as_bytes(),
                project.as_bytes(),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO managed_runs \
             (id, workstream_id, agent_kind, lease_owner, state, sync_after, sync_through, \
              context_delivered, lease_expires_at, started_at) \
             VALUES (?1, ?2, 'codex', 'host:1', 'finished', 3, 7, 1, 10, 1)",
            params![run_id.as_bytes(), workstream.as_bytes()],
        )
        .unwrap();

        run(&mut conn).unwrap();

        let preserved: (String, i64, i64, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT agent_kind, sync_after, sync_through, previous_agent_kind, \
                        previous_native_session_id \
                 FROM managed_runs WHERE id = ?1",
                params![run_id.as_bytes()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(preserved, ("codex".into(), 3, 7, None, None));
        conn.execute(
            "UPDATE managed_runs SET previous_agent_kind = 'claude-code', \
             previous_native_session_id = 'native-1' WHERE id = ?1",
            params![run_id.as_bytes()],
        )
        .unwrap();
        let previous: (String, String) = conn
            .query_row(
                "SELECT previous_agent_kind, previous_native_session_id \
                 FROM managed_runs WHERE id = ?1",
                params![run_id.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(previous, ("claude-code".into(), "native-1".into()));
    }

    /// The one known V62 checksum is repaired before the strict runner sees
    /// it. An arbitrary mismatch still fails closed.
    #[test]
    fn only_the_original_v62_checksum_is_repaired() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();
        let current_checksum = migrations::runner()
            .get_migrations()
            .iter()
            .find(|migration| migration.version() == V62_VERSION)
            .unwrap()
            .checksum();
        assert_ne!(current_checksum, ORIGINAL_V62_CHECKSUM);

        let rewritten = conn
            .execute(
                "UPDATE refinery_schema_history SET checksum = ?1 WHERE version = ?2",
                params![ORIGINAL_V62_CHECKSUM.to_string(), V62_VERSION],
            )
            .unwrap();
        assert_eq!(rewritten, 1, "V62 history row must exist to diverge");

        // A strict runner (refinery's default) rejects the divergence.
        let strict = migrations::runner().run(&mut conn);
        match strict {
            Err(err) => assert!(
                matches!(err.kind(), refinery::error::Kind::DivergentVersion(_, _)),
                "expected DivergentVersion, got: {err:?}"
            ),
            Ok(_) => panic!("strict runner should reject a divergent checksum"),
        }

        run(&mut conn).expect("the released V62 checksum must be repaired");
        let repaired: String = conn
            .query_row(
                "SELECT checksum FROM refinery_schema_history WHERE version = ?1",
                [V62_VERSION],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(repaired, current_checksum.to_string());

        conn.execute(
            "UPDATE refinery_schema_history SET checksum = '0' WHERE version = ?1",
            [V62_VERSION],
        )
        .unwrap();
        match run(&mut conn).unwrap_err() {
            StoreError::Migration(err) => assert!(matches!(
                err.kind(),
                refinery::error::Kind::DivergentVersion(_, _)
            )),
            other => panic!("unknown divergence must remain fatal: {other:?}"),
        }

        conn.execute(
            "UPDATE refinery_schema_history SET checksum = ?1 WHERE version = ?2",
            params![current_checksum.to_string(), V62_VERSION],
        )
        .unwrap();
        let summary = crate::ops::backfill_page_windows(&mut conn).unwrap();
        assert_eq!(summary, crate::ops::PageWindowBackfillSummary::default());
    }

    /// The constant the startup repair matches on must be the refinery
    /// checksum of the SQL actually shipped in v2.2.1 (git blob `28917fb…`),
    /// recomputed here through refinery's own hasher rather than restated,
    /// and it must still differ from the embedded V62 or the repair is dead
    /// weight.
    #[test]
    fn original_v62_checksum_is_pinned_to_the_shipped_v2_2_1_sql() {
        let shipped = include_str!("testdata/V62__page_ingestion_windows.pre776.sql");
        let from_shipped = refinery::Migration::unapplied("V62__page_ingestion_windows", shipped)
            .expect("the v2.2.1 fixture SQL must parse as a migration")
            .checksum();
        assert_eq!(
            from_shipped, ORIGINAL_V62_CHECKSUM,
            "repair constant no longer matches the v2.2.1 V62 SQL"
        );
        let embedded = migrations::runner()
            .get_migrations()
            .iter()
            .find(|migration| migration.version() == V62_VERSION)
            .unwrap()
            .checksum();
        assert_ne!(
            embedded, from_shipped,
            "embedded V62 must still differ from v2.2.1, or the repair is a no-op"
        );
    }

    #[test]
    fn v48_to_v49_replaces_the_decay_tombstone_index_predicate() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 48).unwrap();

        let before: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_pages_evicted'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(before.contains("supersedes IS NULL"), "{before}");

        run(&mut conn).unwrap();
        let after: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_pages_evicted'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            after.contains("workspace_id, project_id, superseded_at"),
            "{after}"
        );
        assert!(after.contains("superseded_at IS NOT NULL"), "{after}");
        assert!(!after.contains("supersedes IS NULL"), "{after}");
    }

    #[test]
    fn project_coordinate_keys_upgrade_and_backfill_without_changing_project_ids() {
        let coordinate_version = migrations::runner()
            .get_migrations()
            .iter()
            .find(|migration| migration.name() == "project_coordinate_keys")
            .map(refinery::Migration::version)
            .unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, coordinate_version - 1).unwrap();
        let workspace_id = [8_u8; 16];
        let project_id = [9_u8; 16];
        conn.execute(
            "INSERT INTO workspaces (id, name, created_at) VALUES (?1, 'acme', 1)",
            params![workspace_id.as_slice()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO projects (id, workspace_id, name, created_at, identity, identity_source) \
             VALUES (?1, ?2, 'api', 1, 'github.com/acme/group/api', 'git_remote')",
            params![project_id.as_slice(), workspace_id.as_slice()],
        )
        .unwrap();

        run(&mut conn).unwrap();
        assert_eq!(crate::project_coordinates::backfill(&mut conn).unwrap(), 1);
        let row: (Vec<u8>, String, String, String) = conn
            .query_row(
                "SELECT id, name, canonical_name, legacy_name FROM projects",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(row.0, project_id);
        assert_eq!(row.1, "api");
        assert_eq!(row.2, "acme-group-api");
        assert_eq!(row.3, "api");
        assert_eq!(crate::project_coordinates::backfill(&mut conn).unwrap(), 0);
    }

    /// Upstream's `UNIQUE (workspace_id, name)` is case-sensitive, so one
    /// workspace may hold both `API` and `api`. The identity index is on
    /// case-folded values, and backfilling `lower(name)` into it failed the
    /// whole upgrade on such an install. Looked up by name, not number: this
    /// migration is renumbered on every upstream sync that adds one of its own.
    #[test]
    fn project_identity_upgrades_a_workspace_with_names_differing_only_in_case() {
        let identity_version = migrations::runner()
            .get_migrations()
            .iter()
            .find(|m| m.name() == "project_identity")
            .map(refinery::Migration::version)
            .expect("the project_identity migration is embedded");

        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, identity_version - 1).unwrap();
        let workspace_id = [7_u8; 16];
        conn.execute(
            "INSERT INTO workspaces (id, name, created_at) VALUES (?1, 'acme', 1)",
            params![workspace_id.as_slice()],
        )
        .unwrap();
        for (id, name) in [([1_u8; 16], "API"), ([2_u8; 16], "api")] {
            conn.execute(
                "INSERT INTO projects (id, workspace_id, name, created_at) VALUES (?1, ?2, ?3, 1)",
                params![id.as_slice(), workspace_id.as_slice(), name],
            )
            .unwrap();
        }

        run(&mut conn).expect("names differing only in case must not fail the upgrade");

        // Both projects survive, and neither is claimed: which one a future
        // `api/` checkout resolves to is not the migration's decision.
        let unclaimed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM projects WHERE identity = '' AND identity_source = ''",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unclaimed, 2);

        // The index still does its job once the resolver claims an identity.
        conn.execute(
            "UPDATE projects SET identity = 'api', identity_source = 'folder_name' WHERE name = 'api'",
            [],
        )
        .unwrap();
        let duplicate = conn.execute(
            "UPDATE projects SET identity = 'api', identity_source = 'folder_name' WHERE name = 'API'",
            [],
        );
        assert!(
            duplicate.is_err(),
            "a claimed identity must stay unique per workspace"
        );
    }
}
