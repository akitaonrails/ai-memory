//! refinery-driven schema migrations.

use crate::error::{StoreError, StoreResult};

refinery::embed_migrations!("migrations");

/// Checksum recorded by refinery for the V62 SQL shipped in v2.2.1.
///
/// The historical SQL is pinned in
/// `testdata/V62__page_ingestion_windows.pre776.sql`; a regression test
/// derives this value through refinery's own checksum implementation.
const ORIGINAL_V62_CHECKSUM: u64 = 16_518_128_245_137_964_566;
const V62_VERSION: u32 = 62;
const V62_NAME: &str = "page_ingestion_windows";

/// Run all pending migrations against an open connection.
///
/// # Errors
/// Propagates the underlying refinery error if a migration fails. When the
/// store is *ahead* of this binary, the error is remapped to
/// [`StoreError::DataSchemaAhead`]. An earlier embedded migration missing from
/// the store's history instead reports [`StoreError::MigrationHistoryGap`].
/// refinery reports both conditions as `MissingVersion`.
pub fn run(conn: &mut rusqlite::Connection) -> StoreResult<()> {
    repair_known_migration_checksums(conn)?;
    migrations::runner()
        .set_abort_divergent(true)
        .set_abort_missing(true)
        .run(conn)
        .map_err(classify_run_error)?;
    Ok(())
}

/// Rewrite only checksums whose SQL bytes are known to be equivalent.
///
/// Windows release builds before v3.1.2 embedded the checked-out migration
/// files with CRLF endings. The SQL and resulting schema are identical to the
/// LF files embedded now, but refinery hashes the raw bytes. V62 also has one
/// older released layout whose equivalent backfill moved to the bounded boot
/// path (#776). Match version, name and the exact historical checksum before
/// changing anything; every other mismatch remains a hard refinery error.
fn repair_known_migration_checksums(conn: &mut rusqlite::Connection) -> StoreResult<()> {
    let has_history: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master \
         WHERE type = 'table' AND name = 'refinery_schema_history')",
        [],
        |row| row.get(0),
    )?;
    if !has_history {
        return Ok(());
    }
    let runner = migrations::runner();
    let mut known = Vec::new();
    for migration in runner.get_migrations() {
        let Some(crlf_checksum) = crlf_checksum(migration) else {
            continue;
        };
        known.push((
            migration.version(),
            migration.name().to_owned(),
            crlf_checksum,
            migration.checksum(),
        ));
    }
    let Some(v62) = runner
        .get_migrations()
        .iter()
        .find(|migration| migration.version() == V62_VERSION)
    else {
        return Ok(());
    };
    known.push((
        V62_VERSION,
        V62_NAME.to_owned(),
        ORIGINAL_V62_CHECKSUM,
        v62.checksum(),
    ));
    let original_v62 = include_str!("testdata/V62__page_ingestion_windows.pre776.sql");
    known.push((
        V62_VERSION,
        V62_NAME.to_owned(),
        checksum_for_sql(V62_VERSION, V62_NAME, &original_v62.replace('\n', "\r\n")),
        v62.checksum(),
    ));

    let tx = conn.transaction()?;
    let mut repaired = 0_usize;
    for (version, name, historical, current) in known {
        repaired += tx.execute(
            "UPDATE refinery_schema_history SET checksum = ?1 \
             WHERE version = ?2 AND name = ?3 AND checksum = ?4",
            rusqlite::params![current.to_string(), version, name, historical.to_string()],
        )?;
    }
    tx.commit()?;
    if repaired > 0 {
        tracing::info!(
            migrations = repaired,
            "repaired known byte-equivalent migration checksums"
        );
    }
    Ok(())
}

fn crlf_checksum(migration: &refinery::Migration) -> Option<u64> {
    let sql = migration.sql()?;
    Some(checksum_for_sql(
        migration.version(),
        migration.name(),
        &sql.replace('\n', "\r\n"),
    ))
}

fn checksum_for_sql(version: u32, name: &str, sql: &str) -> u64 {
    refinery::Migration::unapplied(&format!("V{version}__{name}"), sql)
        .expect("embedded migration identity must remain valid")
        .checksum()
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

/// Translate refinery's two `MissingVersion` cases without confusing a
/// skipped migration with a downgrade. Every other failure passes through.
fn classify_run_error(err: refinery::Error) -> StoreError {
    if let refinery::error::Kind::MissingVersion(migration) = err.kind() {
        let identity = format!("V{} ({})", migration.version(), migration.name());
        // refinery returns the embedded (unapplied) migration when an older
        // version was skipped, and the history row when a build lacks it.
        if migration.applied_on().is_none() {
            return StoreError::MigrationHistoryGap { missing: identity };
        }
        let supported = max_supported_version();
        if migration.version() > supported {
            return StoreError::DataSchemaAhead {
                applied: identity,
                supported,
            };
        }
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

    #[test]
    fn v94_preserves_old_response_rows_and_cascades_context() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run_to(&mut conn, 93).unwrap();
        conn.execute_batch("INSERT INTO workspaces(id,name,created_at) VALUES(zeroblob(16),'usage',1);
            INSERT INTO projects(id,workspace_id,name,created_at) VALUES(zeroblob(16),zeroblob(16),'usage',1);
            INSERT INTO sessions(id,workspace_id,project_id,agent_kind,started_at) VALUES(zeroblob(16),zeroblob(16),zeroblob(16),'codex',1);
            INSERT INTO token_calls(session_id,call_id,input_tokens,output_tokens,cache_write_tokens,cache_read_tokens,observed_at) VALUES(zeroblob(16),'old-response',70,7,0,0,123);").unwrap();
        run(&mut conn).unwrap();
        run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let preserved:(u64,u64,i64,Option<String>,Option<String>,Option<i64>)=conn.query_row("SELECT input_tokens,output_tokens,observed_at,turn_id,native_response_id,response_at FROM token_calls",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
        assert_eq!(preserved, (70, 7, 123, None, None, None));
        conn.execute_batch("INSERT INTO token_call_tools(session_id,call_id,tool_id,tool_name,relation) VALUES(zeroblob(16),'old-response','tool-1','Read','requested');
            DELETE FROM token_calls WHERE call_id='old-response';").unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM token_call_tools", [], |r| r
                .get::<_, u64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn v92_normalizes_codex_cache_once_and_preserves_claude() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run_to(&mut conn, 91).unwrap();
        conn.execute_batch("INSERT INTO workspaces(id,name,created_at) VALUES(zeroblob(16),'usage',1);
            INSERT INTO projects(id,workspace_id,name,created_at) VALUES(zeroblob(16),zeroblob(16),'usage',1);
            INSERT INTO sessions(id,workspace_id,project_id,agent_kind,started_at) VALUES(zeroblob(16),zeroblob(16),zeroblob(16),'codex',1);
            INSERT INTO sessions(id,workspace_id,project_id,agent_kind,started_at) VALUES(x'00000000000000000000000000000001',zeroblob(16),zeroblob(16),'claude-code',1);
            INSERT INTO session_usage(session_id,input_tokens,cache_read_tokens,updated_at) SELECT id,1000,800,1 FROM sessions;").unwrap();
        run(&mut conn).unwrap();
        run(&mut conn).unwrap();
        let codex: u64 = conn
            .query_row(
                "SELECT input_tokens FROM session_usage WHERE session_id=zeroblob(16)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let claude:u64=conn.query_row("SELECT input_tokens FROM session_usage WHERE session_id=x'00000000000000000000000000000001'",[],|r|r.get(0)).unwrap();
        assert_eq!(codex, 200);
        assert_eq!(claude, 1000);
    }

    /// V95 rebuilds `agent_messages` so a message can be held in `editing`.
    /// Existing mail keeps its text, state, order and addressee, the three
    /// indexes come back, and `editing` is the only value the constraint gained.
    #[test]
    fn v95_accepts_the_editing_state_and_keeps_existing_messages() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run_to(&mut conn, 94).unwrap();
        conn.execute_batch(
            "INSERT INTO workspaces(id,name,created_at) VALUES(zeroblob(16),'mail',1);
             INSERT INTO projects(id,workspace_id,name,created_at) VALUES(zeroblob(16),zeroblob(16),'mail',1);
             INSERT INTO agent_messages(id,from_workspace_id,from_project_id,from_agent,to_workspace_id,to_project_id,subject,body,state,created_at,to_agent,from_owner_user)
                 VALUES(x'01000000000000000000000000000000',zeroblob(16),zeroblob(16),'other',zeroblob(16),zeroblob(16),'s','queued','pending',5,'codex','user:sender');
             INSERT INTO agent_messages(id,from_workspace_id,from_project_id,from_agent,to_workspace_id,to_project_id,body,state,created_at,claimed_at,claimed_by_agent)
                 VALUES(x'02000000000000000000000000000000',zeroblob(16),zeroblob(16),'other',zeroblob(16),zeroblob(16),'done','claimed',6,7,'codex');",
        )
        .unwrap();
        run(&mut conn).unwrap();
        run(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let rows: Vec<(String, String, i64, Option<String>)> = conn
            .prepare(
                "SELECT state, body, created_at, to_agent FROM agent_messages ORDER BY created_at",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (
                    "pending".to_owned(),
                    "queued".to_owned(),
                    5,
                    Some("codex".to_owned())
                ),
                ("claimed".to_owned(), "done".to_owned(), 6, None),
            ]
        );
        conn.execute(
            "UPDATE agent_messages SET state='editing' WHERE body='queued'",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "UPDATE agent_messages SET state='held' WHERE body='queued'",
                [],
            )
            .is_err(),
            "the state constraint must still reject unknown values"
        );
        let indexes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND tbl_name='agent_messages' \
                 AND name IN ('idx_agent_messages_inbox','idx_agent_messages_outbox','idx_messages_agent_inbox')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(indexes, 3);
        let edited_at: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('agent_messages') WHERE name='edited_at'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(edited_at, 1);
        let provenance: (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT from_owner_user, subject FROM agent_messages WHERE body='queued'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            provenance,
            (Some("user:sender".to_owned()), Some("s".to_owned())),
            "the sender identity and subject survive the rebuild"
        );
        assert!(
            conn.execute(
                "UPDATE agent_messages SET from_session_id=zeroblob(16) WHERE body='queued'",
                [],
            )
            .is_err(),
            "the sender-session foreign key survives the rebuild"
        );
        conn.execute_batch("INSERT INTO sessions(id,workspace_id,project_id,agent_kind,started_at) VALUES(zeroblob(16),zeroblob(16),zeroblob(16),'codex',1);
            UPDATE agent_messages SET from_session_id=zeroblob(16) WHERE body='queued';").unwrap();
        let dangling: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_check('agent_messages')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dangling, 0);
    }

    #[test]
    fn project_id_pinning_upgrade_preserves_runs_and_failed_prepare_rolls_back() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 95).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let ws = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project = crate::ops::get_or_create_project(&mut conn, &ws, "project", None).unwrap();
        let workstream = ai_memory_core::WorkstreamId::new();
        let run_id = ai_memory_core::ManagedRunId::new();
        conn.execute("INSERT INTO workstreams \
            (id, workspace_id, project_id, repo_fingerprint, worktree_fingerprint, name, cwd, created_at, selected_at, updated_at) \
            VALUES (?1, ?2, ?3, 'repo', 'tree', 'default', '/repo', 1, 1, 1)",
            params![workstream.as_bytes(), ws.as_bytes(), project.as_bytes()]).unwrap();
        conn.execute("INSERT INTO managed_runs \
            (id, workstream_id, agent_kind, lease_owner, state, sync_after, sync_through, context_delivered, lease_expires_at, started_at) \
            VALUES (?1, ?2, 'codex', 'host:1', 'active', 3, 7, 1, ?3, 1)",
            params![run_id.as_bytes(), workstream.as_bytes(), jiff::Timestamp::now().as_microsecond() + 60_000_000]).unwrap();
        run(&mut conn).unwrap();
        let snapshot = |conn: &Connection| {
            conn.query_row(
            "SELECT state, sync_after, sync_through, context_delivered, scope_by_id FROM managed_runs WHERE id = ?1",
            params![run_id.as_bytes()], |row| Ok((row.get::<_,String>(0)?, row.get::<_,i64>(1)?, row.get::<_,i64>(2)?, row.get::<_,bool>(3)?, row.get::<_,bool>(4)?))).unwrap()
        };
        assert_eq!(snapshot(&conn), ("active".into(), 3, 7, true, false));
        conn.execute_batch("CREATE TRIGGER reject_new_run BEFORE INSERT ON managed_runs BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
        let input = crate::PrepareWorkstreamRun {
            workspace_id: ws,
            project_id: project,
            scope_by_id: true,
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "tree".into(),
            cwd: "/repo".into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: Vec::new(),
            selection: crate::WorkstreamSelection::Named("default".into()),
            lease_owner: "host:2".into(),
        };
        assert!(crate::workstream::prepare_run(&mut conn, &input, None, true).is_err());
        assert_eq!(
            snapshot(&conn),
            ("active".into(), 3, 7, true, false),
            "failed prepare must roll back forced expiry and preserve the legacy flag"
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM managed_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        conn.execute_batch("DROP TRIGGER reject_new_run;").unwrap();
        let prepared = crate::workstream::prepare_run(&mut conn, &input, None, true).unwrap();
        let pinned: bool = conn
            .query_row(
                "SELECT scope_by_id FROM managed_runs WHERE id = ?1",
                params![prepared.run_id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(pinned);
        assert_eq!(snapshot(&conn).0, "expired");
    }

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

    #[test]
    fn missing_earlier_migration_reports_a_history_gap_without_applying_anything() {
        let mut conn = Connection::open_in_memory().unwrap();
        // Between v2.3.2 and v2.4, main briefly contained V66 before it
        // received V65 from release/2.4 (#1612).
        let historical: Vec<_> = migrations::runner()
            .get_migrations()
            .iter()
            .filter(|migration| migration.version() <= 66 && migration.version() != 65)
            .cloned()
            .collect();
        refinery::Runner::new(&historical).run(&mut conn).unwrap();
        let before = applied_versions(&conn);
        assert_eq!(before.last(), Some(&66));
        assert!(!before.contains(&65));

        let err = run(&mut conn).unwrap_err();
        let rendered = err.to_string();
        assert!(
            rendered.contains("migration history has a gap"),
            "{rendered}"
        );
        assert!(
            rendered.contains("V65 (page_compacted_marker)"),
            "{rendered}"
        );
        assert!(rendered.contains("not recorded"), "{rendered}");
        assert!(!rendered.contains("newer than"), "{rendered}");
        assert_eq!(applied_versions(&conn), before);
        let compacted_column: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('pages') WHERE name = 'compacted_at'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(compacted_column, 0, "a refused open must not apply V65");
    }

    #[test]
    fn an_ordinary_older_store_migrates_without_a_history_gap() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 64).unwrap();
        run(&mut conn).unwrap();
        let applied = applied_versions(&conn);
        assert!(applied.contains(&65) && applied.contains(&66));
        assert_eq!(applied.last(), Some(&i64::from(max_supported_version())));
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

    /// V88 expands the sessions enum for Qwen Code. Existing sessions and
    /// dependent observations survive the table rebuild, while the old V87
    /// constraint rejects the new value and the migrated schema accepts it.
    #[test]
    fn v87_to_v88_preserves_sessions_and_observations_and_accepts_qwen_code() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 87).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace_id = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id =
            crate::ops::get_or_create_project(&mut conn, &workspace_id, "project", None).unwrap();
        let existing = SessionId::new();
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, actor_user) \
             VALUES (?1, ?2, ?3, 'ibm-bob', '/repo', 1, 'user:alice')",
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
                 VALUES (?1, ?2, ?3, 'qwen-code', 1)",
                params![
                    SessionId::new().as_bytes(),
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                ],
            )
            .is_err(),
            "the V87 CHECK must reject qwen-code"
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
            ("ibm-bob".into(), "/repo".into(), Some("user:alice".into()))
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
                agent_kind: AgentKind::QwenCode,
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

    /// V89 expands the persisted session kind without losing the session
    /// graph rebuilt by V88.
    #[test]
    fn v88_to_v89_preserves_sessions_and_accepts_qoder() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 88).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let workspace_id = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let project_id =
            crate::ops::get_or_create_project(&mut conn, &workspace_id, "project", None).unwrap();
        let existing = SessionId::new();
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, actor_user) \
             VALUES (?1, ?2, ?3, 'qwen-code', '/repo', 1, 'user:alice')",
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
                 VALUES (?1, ?2, ?3, 'qoder', 1)",
                params![
                    SessionId::new().as_bytes(),
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                ],
            )
            .is_err(),
            "the V88 CHECK must reject qoder"
        );

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
                "qwen-code".into(),
                "/repo".into(),
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
                agent_kind: AgentKind::Qoder,
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
    fn v88_to_v89_preserves_existing_sessions_and_keeps_missing_usage_unknown() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 88).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let before = applied_versions(&conn);
        let workspace = crate::ops::get_or_create_workspace(&mut conn, "upgrade").unwrap();
        let project =
            crate::ops::get_or_create_project(&mut conn, &workspace, "app", None).unwrap();
        let session = SessionId::new();
        crate::ops::begin_session(
            &mut conn,
            &NewSession {
                occurred_at: None,
                id: session,
                workspace_id: workspace,
                project_id: project,
                agent_kind: AgentKind::Codex,
                cwd: Some("/repo".into()),
                actor_user: Some("user:alice".into()),
            },
        )
        .unwrap();

        run_to(&mut conn, 89).unwrap();
        let mut expected = before;
        expected.push(89);
        assert_eq!(applied_versions(&conn), expected);
        run(&mut conn).unwrap();
        let preserved: (String, String, String) = conn
            .query_row(
                "SELECT agent_kind, cwd, actor_user FROM sessions WHERE id = ?1",
                params![session.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            preserved,
            ("codex".into(), "/repo".into(), "user:alice".into())
        );
        let missing: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_usage", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            missing, 0,
            "upgrading must not invent usage for existing sessions"
        );

        let mut report = ai_memory_core::NewSessionUsage {
            session_id: SessionId::new(),
            input_tokens: 12,
            output_tokens: 4,
            cache_write_tokens: 0,
            cache_read_tokens: 2,
            model: Some("fixture-model".into()),
        };
        assert!(crate::ops::upsert_session_usage(&conn, &report).is_err());
        report.session_id = session;
        crate::ops::upsert_session_usage(&conn, &report).unwrap();
        conn.execute(
            "DELETE FROM sessions WHERE id = ?1",
            params![session.as_bytes()],
        )
        .unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_usage", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0, "usage must not outlive its session");
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            0
        );
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

    /// The repair constant comes from the released SQL, not a remembered
    /// value that could silently drift from its provenance.
    #[test]
    fn original_v62_checksum_matches_the_released_sql() {
        let released_sql = include_str!("testdata/V62__page_ingestion_windows.pre776.sql");
        let checksum = checksum_for_sql(V62_VERSION, V62_NAME, released_sql);

        assert_eq!(checksum, ORIGINAL_V62_CHECKSUM);
        let embedded = migrations::runner()
            .get_migrations()
            .iter()
            .find(|migration| migration.version() == V62_VERSION)
            .expect("V62 must remain embedded")
            .checksum();
        assert_ne!(
            embedded, checksum,
            "the V62 repair is unnecessary if the embedded SQL matches v2.2.1"
        );
    }

    /// The reporter's three samples pin the exact Windows-native failure: the
    /// checksums are the current embedded SQL with only LF changed to CRLF.
    #[test]
    fn windows_release_checksums_match_crlf_embedded_sql() {
        let runner = migrations::runner();
        for (version, reported) in [
            (1, 17_285_868_314_470_184_417),
            (62, 3_297_905_123_477_343_401),
            (83, 11_621_067_245_702_300_616),
        ] {
            let migration = runner
                .get_migrations()
                .iter()
                .find(|migration| migration.version() == version)
                .unwrap();
            assert_eq!(crlf_checksum(migration), Some(reported), "V{version}");
        }
    }

    /// Every known byte-equivalent checksum is repaired before the strict
    /// runner sees it. An arbitrary mismatch still fails closed.
    #[test]
    fn only_known_equivalent_migration_checksums_are_repaired() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();
        let runner = migrations::runner();
        let v62 = runner
            .get_migrations()
            .iter()
            .find(|migration| migration.version() == V62_VERSION)
            .unwrap();
        let current_checksum = v62.checksum();
        assert_ne!(current_checksum, ORIGINAL_V62_CHECKSUM);

        for version in [1, 62, 83] {
            let migration = runner
                .get_migrations()
                .iter()
                .find(|migration| migration.version() == version)
                .unwrap();
            let rewritten = conn
                .execute(
                    "UPDATE refinery_schema_history SET checksum = ?1 WHERE version = ?2",
                    params![crlf_checksum(migration).unwrap().to_string(), version],
                )
                .unwrap();
            assert_eq!(rewritten, 1, "V{version} history row must exist");
        }

        // A strict runner (refinery's default) rejects the divergence.
        let strict = migrations::runner().run(&mut conn);
        match strict {
            Err(err) => assert!(
                matches!(err.kind(), refinery::error::Kind::DivergentVersion(_, _)),
                "expected DivergentVersion, got: {err:?}"
            ),
            Ok(_) => panic!("strict runner should reject a divergent checksum"),
        }

        run(&mut conn).expect("the Windows-native checksums must be repaired");
        for version in [1, 62, 83] {
            let expected = runner
                .get_migrations()
                .iter()
                .find(|migration| migration.version() == version)
                .unwrap()
                .checksum()
                .to_string();
            let repaired: String = conn
                .query_row(
                    "SELECT checksum FROM refinery_schema_history WHERE version = ?1",
                    [version],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(repaired, expected, "V{version}");
        }

        conn.execute(
            "UPDATE refinery_schema_history SET checksum = ?1 WHERE version = ?2",
            params![ORIGINAL_V62_CHECKSUM.to_string(), V62_VERSION],
        )
        .unwrap();
        run(&mut conn).expect("the released V62 checksum must be repaired");
        let repaired: String = conn
            .query_row(
                "SELECT checksum FROM refinery_schema_history WHERE version = ?1",
                [V62_VERSION],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(repaired, current_checksum.to_string());

        let original_crlf = checksum_for_sql(
            V62_VERSION,
            V62_NAME,
            &include_str!("testdata/V62__page_ingestion_windows.pre776.sql").replace('\n', "\r\n"),
        );
        conn.execute(
            "UPDATE refinery_schema_history SET checksum = ?1 WHERE version = ?2",
            params![original_crlf.to_string(), V62_VERSION],
        )
        .unwrap();
        run(&mut conn).expect("the Windows-native original V62 checksum must be repaired");

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
    #[test]
    fn v97_adds_processing_tables_and_keeps_the_fence_out_of_the_shared_schema() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_to(&mut conn, 96).unwrap();
        let before = applied_versions(&conn);
        run_to(&mut conn, 97).unwrap();
        let mut expected = before;
        expected.push(97);
        assert_eq!(applied_versions(&conn), expected);
        for (kind, name) in [
            ("table", "processing_jobs"),
            ("table", "processing_job_observations"),
            ("table", "processing_job_nodes"),
            ("table", "processing_job_effects"),
            ("index", "idx_processing_jobs_equivalence"),
            ("trigger", "observations_processing_invalidate_au"),
            ("trigger", "observations_processing_invalidate_ad"),
        ] {
            assert_eq!(schema_object_count(&conn, kind, name), 1, "missing {name}");
        }
        // The armed claim must never live in the file every writer shares.
        assert_eq!(
            schema_object_count(&conn, "table", "processing_write_fence"),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE '%processing_fence%'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
    }
}
