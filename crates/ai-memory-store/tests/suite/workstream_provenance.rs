//! Provenance uses the configured privacy strip on writes and legacy reads.

use ai_memory_core::{AgentKind, NewWorkstreamEvent, WorkstreamEventKind};
use ai_memory_store::{FinishWorkstreamRun, PrepareWorkstreamRun, Store, WorkstreamSelection};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

fn event(id: &str, source: &str, metadata: Value) -> NewWorkstreamEvent {
    NewWorkstreamEvent {
        event_id: id.into(),
        agent: AgentKind::Codex,
        native_session_id: "native-1".into(),
        source_record_id: Some(source.into()),
        kind: WorkstreamEventKind::ToolResult,
        role: Some("tool".into()),
        content: "portable provenance sentinel".into(),
        occurred_at: None,
        metadata,
    }
}

async fn prepare(store: &Store) -> ai_memory_store::PreparedWorkstreamRun {
    let workspace_id = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let project_id = store
        .writer
        .get_or_create_project(workspace_id, "managed", None)
        .await
        .unwrap();
    store
        .writer
        .prepare_workstream_run(PrepareWorkstreamRun {
            scope_by_id: false,
            workspace_id,
            project_id,
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            cwd: "/repo".into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: Vec::new(),
            selection: WorkstreamSelection::Current,
            lease_owner: "test".into(),
        })
        .await
        .unwrap()
}

fn finish(
    run_id: ai_memory_core::ManagedRunId,
    events: Vec<NewWorkstreamEvent>,
) -> FinishWorkstreamRun {
    FinishWorkstreamRun {
        sanitizer: ai_memory_core::Sanitizer::default(),
        run_id,
        native_session_id: Some("native-1".into()),
        source_cursor: None,
        events,
        complete: false,
        segment_path: None,
        exit_code: None,
    }
}

#[tokio::test]
async fn workstream_provenance_is_bounded_scrubbed_and_idempotent_before_storage() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let run = prepare(&store).await;
    // The credential crosses the cap: truncate-before-scrub would leak it.
    let hostile = format!("{}sk-{}\u{1b}[31m", "é".repeat(250), "a".repeat(40));
    let oversized = format!("{}-one", "é".repeat(600));
    let batch = finish(
        run.run_id,
        vec![
            event(
                "hostile",
                &hostile,
                json!({
                    "tool": "Read", "tool_call_id": "call-1", "parent_id": "record-0",
                    "is_error": false, "status": "completed", "summary_type": "branch_summary",
                    "tool_use_id": "sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "exit_code": 0, "loss_count": 2,
                    "unknown": {"prompt": "private dump"}, "authority": "root",
                }),
            ),
            event(
                "oversized-one",
                &oversized,
                json!({"tool": ["private dump"]}),
            ),
            event(
                "oversized-two",
                &oversized.replace("-one", "-two"),
                json!({}),
            ),
            event("legitimate", "record-1", json!({"tool_call_id": "call-1"})),
        ],
    );
    assert_eq!(
        store
            .writer
            .finish_workstream_run(local_authority(), batch.clone())
            .await
            .unwrap()
            .imported_events,
        4
    );
    assert_eq!(
        store
            .writer
            .finish_workstream_run(local_authority(), batch)
            .await
            .unwrap()
            .imported_events,
        0
    );
    let conn = Connection::open(store.db_path()).unwrap();
    let (source, metadata): (String, String) = conn.query_row(
        "SELECT source_record_id, metadata_json FROM workstream_events WHERE event_id = 'hostile'",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert!(!source.contains("sk-"), "source id leaked a credential");
    assert!(!source.contains('\u{1b}'));
    assert!(source.len() <= 512);
    let metadata: Value = serde_json::from_str(&metadata).unwrap();
    assert!(metadata.get("unknown").is_none());
    assert!(metadata.get("authority").is_none());
    assert_eq!(metadata["tool"], "Read");
    assert_eq!(metadata["tool_call_id"], "call-1");
    assert_eq!(metadata["parent_id"], "record-0");
    assert_eq!(metadata["summary_type"], "branch_summary");
    assert_eq!(metadata["is_error"], false);
    assert_eq!(metadata["status"], "completed");
    assert_eq!(metadata["exit_code"], 0);
    assert_eq!(metadata["loss_count"], 2);
    assert!(!metadata["tool_use_id"].as_str().unwrap().contains("sk-"));
    for query in ["", "portable"] {
        let hits = store
            .reader
            .search_workstream_events(
                run.workstream_id,
                query.into(),
                10,
                ai_memory_core::Sanitizer::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            4,
            "distinct event ids must survive source-id truncation"
        );
        let hit = serde_json::to_value(
            hits.iter()
                .find(|hit| hit.event_id == "legitimate")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(hit["source_record_id"], "record-1");
        assert_eq!(hit["metadata"]["tool_call_id"], "call-1");
        for hit in hits
            .iter()
            .filter(|hit| hit.event_id.starts_with("oversized"))
        {
            assert!(hit.source_record_id.as_ref().unwrap().len() <= 512);
            assert!(hit.metadata.is_null());
        }
    }
}

#[tokio::test]
async fn workstream_provenance_legacy_reads_strip_secrets_and_unknown_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let run = prepare(&store).await;
    store
        .writer
        .finish_workstream_run(
            local_authority(),
            finish(run.run_id, vec![event("legacy", "record-1", json!({}))]),
        )
        .await
        .unwrap();
    // Simulate rows written before provenance was scrubbed at ingress.
    let conn = Connection::open(store.db_path()).unwrap();
    conn.execute("UPDATE workstream_events SET source_record_id = ?1, metadata_json = ?2 WHERE event_id = 'legacy'",
        params!["sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", json!({"parent_id": "record-0", "tool_call_id": "sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "dump": "private"}).to_string()]).unwrap();
    for query in ["", "portable"] {
        let hits = store
            .reader
            .search_workstream_events(
                run.workstream_id,
                query.into(),
                10,
                ai_memory_core::Sanitizer::default(),
            )
            .await
            .unwrap();
        let hit = serde_json::to_value(&hits[0]).unwrap();
        assert!(
            hit["source_record_id"].as_str().is_some(),
            "legacy provenance must be returned safely"
        );
        assert!(!hit.to_string().contains("sk-"));
        assert_eq!(hit["metadata"]["parent_id"], "record-0");
        assert!(hit["metadata"].get("dump").is_none());
    }
    for malformed in ["not JSON".to_string(), "x".repeat(16 * 1024 + 1)] {
        conn.execute(
            "UPDATE workstream_events SET metadata_json = ?1 WHERE event_id = 'legacy'",
            params![malformed],
        )
        .unwrap();
        let hits = store
            .reader
            .search_workstream_events(
                run.workstream_id,
                "portable".into(),
                10,
                ai_memory_core::Sanitizer::default(),
            )
            .await
            .unwrap();
        assert!(hits[0].metadata.is_null());
        assert!(!hits[0].source_record_id.as_ref().unwrap().contains("sk-"));
    }
}

#[tokio::test]
async fn workstream_provenance_reused_event_id_retains_data_and_refuses_identity_change() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let run = prepare(&store).await;
    let first = event("stable-id", "record-1", json!({"tool_call_id": "call-1"}));
    store
        .writer
        .finish_workstream_run(local_authority(), finish(run.run_id, vec![first.clone()]))
        .await
        .unwrap();
    let mut reused = first.clone();
    reused.source_record_id = Some("record-2".into());
    reused.content = "different source content".into();
    reused.metadata = json!({"tool_call_id": "call-2"});
    reused.role = Some("assistant".into());
    reused.occurred_at = Some("2026-10-02T00:00:00Z".into());
    assert_eq!(
        store
            .writer
            .finish_workstream_run(local_authority(), finish(run.run_id, vec![reused.clone()]))
            .await
            .unwrap()
            .imported_events,
        0
    );
    reused.kind = WorkstreamEventKind::Message;
    assert!(matches!(
        store
            .writer
            .finish_workstream_run(
                local_authority(),
                finish(
                    run.run_id,
                    vec![event("new-before-collision", "record-3", json!({})), reused,]
                )
            )
            .await,
        Err(ai_memory_store::StoreError::InvalidState(_))
    ));
    let hits = store
        .reader
        .search_workstream_events(
            run.workstream_id,
            "".into(),
            10,
            ai_memory_core::Sanitizer::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "the rejected batch must roll back earlier inserts"
    );
    assert_eq!(hits[0].content, first.content);
    assert_eq!(hits[0].source_record_id, first.source_record_id);
    assert_eq!(hits[0].metadata, first.metadata);
    assert_eq!(hits[0].role, first.role);
    assert_eq!(hits[0].occurred_at, first.occurred_at);
    assert_eq!(
        store
            .writer
            .finish_workstream_run(local_authority(), finish(run.run_id, vec![first]))
            .await
            .unwrap()
            .imported_events,
        0
    );
}

#[tokio::test]
async fn workstream_provenance_finished_run_ignores_late_events() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let run = prepare(&store).await;
    let first = event("stable-id", "record-1", json!({"tool_call_id": "call-1"}));
    let mut batch = finish(run.run_id, vec![first]);
    batch.complete = true;
    assert_eq!(
        store
            .writer
            .finish_workstream_run(local_authority(), batch.clone())
            .await
            .unwrap()
            .imported_events,
        1
    );
    assert_eq!(
        store
            .writer
            .finish_workstream_run(local_authority(), batch.clone())
            .await
            .unwrap()
            .imported_events,
        0
    );
    batch.events[0].source_record_id = Some("record-2".into());
    batch
        .events
        .push(event("late-new-event", "record-3", json!({})));
    let result = store
        .writer
        .finish_workstream_run(local_authority(), batch)
        .await
        .unwrap();
    assert_eq!(result.imported_events, 0);
    assert_eq!(result.latest_sequence, 1);
    let hits = store
        .reader
        .search_workstream_events(
            run.workstream_id,
            String::new(),
            10,
            ai_memory_core::Sanitizer::default(),
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].source_record_id.as_deref(), Some("record-1"));
}

#[tokio::test]
async fn workstream_provenance_replay_survives_sanitizer_changes() {
    let token = format!("sk-{}", "a".repeat(40));
    for (source, old_config, new_config) in [
        (
            "private-correlation".to_string(),
            ai_memory_core::SanitizeConfig {
                extra_patterns: Vec::new(),
                allowlist: Vec::new(),
            },
            ai_memory_core::SanitizeConfig {
                extra_patterns: vec!["private-correlation".into()],
                allowlist: Vec::new(),
            },
        ),
        (
            token.clone(),
            ai_memory_core::SanitizeConfig {
                extra_patterns: Vec::new(),
                allowlist: vec![token],
            },
            ai_memory_core::SanitizeConfig {
                extra_patterns: Vec::new(),
                allowlist: Vec::new(),
            },
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let run = prepare(&store).await;
        let mut original = event("stable-id", &source, json!({"parent_id": source}));
        original.content = format!("portable {source}");
        let mut batch = finish(run.run_id, vec![original.clone()]);
        batch.sanitizer = ai_memory_core::Sanitizer::new(&old_config).unwrap();
        batch.events[0].content = batch.sanitizer.scrub(&original.content);
        assert_eq!(
            store
                .writer
                .finish_workstream_run(local_authority(), batch.clone())
                .await
                .unwrap()
                .imported_events,
            1
        );
        let conn = Connection::open(store.db_path()).unwrap();
        let read_raw = || {
            conn.query_row(
            "SELECT content, source_record_id, metadata_json FROM workstream_events WHERE event_id = 'stable-id'",
            [], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
        ).unwrap()
        };
        let first = read_raw();
        batch.sanitizer = ai_memory_core::Sanitizer::new(&new_config).unwrap();
        batch.events[0].content = batch.sanitizer.scrub(&original.content);
        assert_ne!(batch.events[0].content, first.0);
        assert_eq!(
            store
                .writer
                .finish_workstream_run(local_authority(), batch.clone())
                .await
                .unwrap()
                .imported_events,
            0
        );
        assert_eq!(
            read_raw(),
            first,
            "replay must retain the first stored version"
        );
        let hits = store
            .reader
            .search_workstream_events(
                run.workstream_id,
                String::new(),
                10,
                batch.sanitizer.clone(),
            )
            .await
            .unwrap();
        assert!(!hits[0].source_record_id.as_ref().unwrap().contains(&source));
        assert!(!hits[0].metadata.to_string().contains(&source));
        batch.events[0].kind = WorkstreamEventKind::Message;
        assert!(matches!(
            store
                .writer
                .finish_workstream_run(local_authority(), batch)
                .await,
            Err(ai_memory_store::StoreError::InvalidState(_))
        ));
    }
}

#[tokio::test]
async fn workstream_provenance_replay_preserves_legacy_rows_above_bounds() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let run = prepare(&store).await;
    let mut original = event(
        "legacy",
        &"s".repeat(2048),
        json!({"parent_id": "p".repeat(2048), "dump": "x".repeat(20 * 1024)}),
    );
    original.content = "portable ".repeat(4096);
    store
        .writer
        .finish_workstream_run(
            local_authority(),
            finish(run.run_id, vec![original.clone()]),
        )
        .await
        .unwrap();
    let conn = Connection::open(store.db_path()).unwrap();
    // Legacy data can exceed today's write bounds; replay must not rewrite it.
    conn.execute("UPDATE workstream_events SET content = ?1, source_record_id = ?2, metadata_json = ?3 WHERE event_id = 'legacy'",
        params![original.content, original.source_record_id, original.metadata.to_string()]).unwrap();
    assert_eq!(
        store
            .writer
            .finish_workstream_run(
                local_authority(),
                finish(run.run_id, vec![original.clone()])
            )
            .await
            .unwrap()
            .imported_events,
        0
    );
    let raw: (String, String, String) = conn.query_row(
        "SELECT content, source_record_id, metadata_json FROM workstream_events WHERE event_id = 'legacy'",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    assert_eq!(
        raw,
        (
            original.content,
            original.source_record_id.unwrap(),
            original.metadata.to_string()
        )
    );
    for query in ["", "portable"] {
        let hits = store
            .reader
            .search_workstream_events(
                run.workstream_id,
                query.into(),
                10,
                ai_memory_core::Sanitizer::default(),
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source_record_id.as_ref().unwrap().len(), 512);
        assert!(hits[0].metadata.is_null());
    }
}

#[tokio::test]
async fn workstream_provenance_retry_with_marker_matching_pattern_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let run = prepare(&store).await;
    let mut batch = finish(
        run.run_id,
        vec![event(
            "stable-id",
            "custom-source",
            json!({"parent_id": "custom-parent"}),
        )],
    );
    // Valid operator patterns can also match the scrubber's own markers.
    batch.sanitizer = ai_memory_core::Sanitizer::new(&ai_memory_core::SanitizeConfig {
        extra_patterns: vec!["custom".into()],
        allowlist: Vec::new(),
    })
    .unwrap();
    assert_eq!(
        store
            .writer
            .finish_workstream_run(local_authority(), batch.clone())
            .await
            .unwrap()
            .imported_events,
        1
    );
    assert_eq!(
        store
            .writer
            .finish_workstream_run(local_authority(), batch)
            .await
            .unwrap()
            .imported_events,
        0
    );
}

/// Defense in depth at the store boundary (#1113): even a caller that hands
/// `finish_workstream_run` an unscrubbed event gets the credential scrubbed
/// before the 16 KiB content cap, so a secret straddling the cap is never
/// cut into an unmatchable prefix and persisted verbatim.
#[tokio::test]
async fn workstream_content_is_scrubbed_before_the_store_cap() {
    const CONTENT_CAP: usize = 16 * 1024;
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let run = prepare(&store).await;
    // The key starts 18 bytes before the truncation cut (the cap minus the
    // ellipsis reserve): capping first would keep `sk-` plus 15 characters,
    // one short of the 16 the pattern needs, leaking the prefix verbatim.
    // The key follows a space, since the rule skips an `sk-` glued to a
    // letter. The space before the tail keeps the greedy key pattern from swallowing
    // the tail, so the scrubbed text still crosses the cap.
    let mut hostile = event("straddling", "record-1", json!({}));
    hostile.content = format!(
        "{} sk-{} {}",
        "x".repeat(CONTENT_CAP - 22),
        "A".repeat(40),
        "y".repeat(2 * 1024),
    );
    let mut control = event("control", "record-2", json!({}));
    control.content = "plain ledger note".into();
    assert_eq!(
        store
            .writer
            .finish_workstream_run(
                local_authority(),
                finish(run.run_id, vec![hostile, control])
            )
            .await
            .unwrap()
            .imported_events,
        2
    );
    let conn = Connection::open(store.db_path()).unwrap();
    let content: String = conn
        .query_row(
            "SELECT content FROM workstream_events WHERE event_id = 'straddling'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        content.contains("[REDACTED:api_key]"),
        "straddling credential must be redacted before the cap: {content:?}"
    );
    assert!(
        !content.contains("sk-"),
        "cap-straddling credential prefix leaked"
    );
    assert!(
        content.ends_with('…'),
        "scrubbed content must still cross the cap and be truncated"
    );
    assert!(
        content.len() <= CONTENT_CAP,
        "stored content exceeds the cap: {} bytes",
        content.len()
    );
    let control: String = conn
        .query_row(
            "SELECT content FROM workstream_events WHERE event_id = 'control'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(control, "plain ledger note");
}

/// A single-user (no database users) caller: the finish gate admits it, so
/// these provenance tests exercise only the storage boundary.
fn local_authority() -> ai_memory_store::ManagedRunAuthority {
    ai_memory_store::ManagedRunAuthority::from_auth(
        ai_memory_core::AuthLevel::Anonymous,
        None,
        None,
        &ai_memory_core::ActorContext::anonymous(),
        false,
    )
}
