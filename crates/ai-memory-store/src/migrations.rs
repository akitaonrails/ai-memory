//! refinery-driven schema migrations.

use crate::error::{StoreError, StoreResult};

refinery::embed_migrations!("migrations");

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
    migrations::runner().run(conn).map_err(classify_run_error)?;
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
}
