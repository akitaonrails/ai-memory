//! Response counters and the session/agent totals consumed by the dashboard.

use ai_memory_core::{
    AgentKind, NewSession, OwnerFilter, ProjectId, SessionId, TokenCall, WorkspaceId,
};
use ai_memory_store::Store;

async fn session(
    store: &Store,
    ws: WorkspaceId,
    project: ProjectId,
    agent_kind: AgentKind,
) -> SessionId {
    let id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id,
            workspace_id: ws,
            project_id: project,
            agent_kind,
            cwd: None,
            actor_user: None,
            occurred_at: None,
            origin: None,
        })
        .await
        .unwrap();
    id
}

fn call(input: u64, output: u64) -> TokenCall {
    TokenCall {
        context: None,
        call_id: "same-native-response-id".into(),
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: 20,
        cache_write_tokens: 10,
        model: Some("fixture-model".into()),
    }
}

#[tokio::test]
async fn token_calls_keep_partial_reports_monotonic_in_rows_and_aggregates() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace("team").await.unwrap();
    let project = store
        .writer
        .get_or_create_project(ws, "app", None)
        .await
        .unwrap();
    let sid = session(&store, ws, project, AgentKind::Codex).await;
    store
        .writer
        .upsert_token_calls(sid, vec![call(100, 30)])
        .await
        .unwrap();
    let mut partial = call(120, 10);
    partial.cache_read_tokens = 25;
    partial.cache_write_tokens = 5;
    partial.model = None;
    store
        .writer
        .upsert_token_calls(sid, vec![partial.clone(), partial])
        .await
        .unwrap();
    store
        .writer
        .upsert_token_calls(sid, vec![call(80, 5)])
        .await
        .unwrap();

    let report = store
        .reader
        .session_token_usage(ws, project, sid, OwnerFilter::Any)
        .await
        .unwrap();
    assert_eq!(
        (
            report.call_count,
            report.input,
            report.output,
            report.cache_read,
            report.cache_write
        ),
        (1, 120, 30, 25, 10)
    );
    assert_eq!(report.calls.len(), 1);
    let row = &report.calls[0];
    assert_eq!(
        (row.input, row.output, row.cache_read, row.cache_write),
        (120, 30, 25, 10)
    );
    assert_eq!(row.model, "fixture-model");
    let aggregate = store
        .reader
        .total_session_usage(ws, project, OwnerFilter::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            aggregate.input_tokens,
            aggregate.output_tokens,
            aggregate.cache_read_tokens,
            aggregate.cache_write_tokens
        ),
        (120, 30, 25, 10)
    );
    let current = store
        .reader
        .session_summary_scoped(ws, project, sid, OwnerFilter::Any)
        .await
        .unwrap()
        .unwrap();
    assert!(
        current.ended_at.is_none(),
        "Live reporting must not depend on session-end"
    );
}

#[tokio::test]
async fn token_calls_group_by_session_and_agent_without_merging_foreign_responses() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace("team").await.unwrap();
    let project = store
        .writer
        .get_or_create_project(ws, "app", None)
        .await
        .unwrap();
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();
    let mut local = Vec::new();
    for (scope, agent, input) in [
        (project, AgentKind::Codex, 100),
        (project, AgentKind::Codex, 200),
        (project, AgentKind::ClaudeCode, 300),
        (other, AgentKind::Codex, 900),
    ] {
        let sid = session(&store, ws, scope, agent).await;
        // Native ids are only unique within a session; replay each report too.
        store
            .writer
            .upsert_token_calls(sid, vec![call(input, 20), call(input, 20)])
            .await
            .unwrap();
        if scope == project {
            local.push((sid, input));
        }
    }
    let project_usage = store
        .reader
        .token_usage_report(ws, project, OwnerFilter::Any)
        .await
        .unwrap();
    assert_eq!(
        (
            project_usage.call_count,
            project_usage.input,
            project_usage.output
        ),
        (3, 600, 60)
    );
    for (sid, input) in local {
        let ledger = store
            .reader
            .session_token_usage(ws, project, sid, OwnerFilter::Any)
            .await
            .unwrap();
        assert_eq!(
            (ledger.call_count, ledger.input, ledger.output),
            (1, input, 20)
        );
    }
    let agents = store
        .reader
        .agent_usage(ws, project, OwnerFilter::Any)
        .await
        .unwrap();
    let codex = agents.iter().find(|a| a.agent == "codex").unwrap();
    assert_eq!(
        (
            codex.sessions,
            codex.measured_sessions,
            codex.calls,
            codex.input,
            codex.output
        ),
        (2, 2, 2, 300, 40)
    );
    let claude = agents.iter().find(|a| a.agent == "claude-code").unwrap();
    assert_eq!((claude.sessions, claude.calls, claude.input), (1, 1, 300));
    let foreign = store
        .reader
        .token_usage_report(ws, other, OwnerFilter::Any)
        .await
        .unwrap();
    assert_eq!((foreign.call_count, foreign.input), (1, 900));
}

#[tokio::test]
async fn token_call_queries_search_and_order_before_pagination_with_scope_and_owner_guards() {
    use ai_memory_store::{TokenCallOrder, TokenCallQuery};
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace("team").await.unwrap();
    let project = store
        .writer
        .get_or_create_project(ws, "app", None)
        .await
        .unwrap();
    let sid = session(&store, ws, project, AgentKind::Codex).await;
    let mut rows = (0..56)
        .map(|n| {
            let mut c = call(1000 + n, 10);
            c.call_id = format!("bulk-{n}");
            c
        })
        .collect::<Vec<_>>();
    let mut tiny = call(1, 1);
    tiny.call_id = "tiny-newest".into();
    tiny.model = Some("unique-tiny-model".into());
    rows.push(tiny.clone());
    store.writer.upsert_token_calls(sid, rows).await.unwrap();
    let conn = rusqlite::Connection::open(tmp.path().join("db/memory.sqlite")).unwrap();
    conn.execute("UPDATE token_calls SET observed_at=CASE WHEN call_id='tiny-newest' THEN 2000000 ELSE 1000000 END",[]).unwrap();
    let read = |query, owner| {
        store
            .reader
            .token_usage_report_page(ws, project, owner, Some(sid), query)
    };
    let largest = read(TokenCallQuery::default(), OwnerFilter::Any)
        .await
        .unwrap();
    assert_eq!(
        (
            largest.call_count,
            largest.matching_calls,
            largest.calls.len()
        ),
        (57, 57, 50)
    );
    assert!(!largest.calls.iter().any(|c| c.call_id == "tiny-newest"));
    let recent = read(
        TokenCallQuery {
            order: TokenCallOrder::Recent,
            ..Default::default()
        },
        OwnerFilter::Any,
    )
    .await
    .unwrap();
    assert_eq!(recent.calls[0].call_id, "tiny-newest");
    let q = TokenCallQuery {
        search: "UNIQUE-tiny-model".into(),
        ..Default::default()
    };
    let found = read(q.clone(), OwnerFilter::Any).await.unwrap();
    assert_eq!(
        (found.call_count, found.matching_calls, found.calls.len()),
        (57, 1, 1)
    );
    assert_eq!(found.calls[0].call_id, "tiny-newest");
    let tail = read(
        TokenCallQuery {
            offset: 50,
            ..Default::default()
        },
        OwnerFilter::Any,
    )
    .await
    .unwrap();
    assert_eq!(tail.calls.len(), 7);
    assert!(tail.calls.iter().any(|c| c.call_id == "tiny-newest"));
    let signals = read(
        TokenCallQuery {
            signals_only: true,
            ..Default::default()
        },
        OwnerFilter::Any,
    )
    .await
    .unwrap();
    assert!(signals.calls.is_empty());
    let literal = read(
        TokenCallQuery {
            search: "%_".into(),
            ..Default::default()
        },
        OwnerFilter::Any,
    )
    .await
    .unwrap();
    assert_eq!(literal.matching_calls, 0);
    let hidden = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: hidden,
            workspace_id: ws,
            project_id: project,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: Some(ai_memory_core::IdentityKey::User("bob".into()).storage_key()),
            occurred_at: None,
            origin: None,
        })
        .await
        .unwrap();
    store
        .writer
        .upsert_token_calls(hidden, vec![tiny.clone()])
        .await
        .unwrap();
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();
    let foreign = session(&store, ws, other, AgentKind::Codex).await;
    store
        .writer
        .upsert_token_calls(foreign, vec![tiny])
        .await
        .unwrap();
    let mine = store
        .reader
        .token_usage_report_page(
            ws,
            project,
            OwnerFilter::User(ai_memory_core::IdentityKey::User("alice".into()).storage_key()),
            None,
            q.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        (mine.call_count, mine.matching_calls, mine.calls.len()),
        (57, 1, 1)
    );
    let root = store
        .reader
        .token_usage_report_page(ws, project, OwnerFilter::Any, None, q.clone())
        .await
        .unwrap();
    assert_eq!((root.call_count, root.matching_calls), (58, 2));
    let wrong = store
        .reader
        .token_usage_report_page(ws, project, OwnerFilter::Any, Some(foreign), q)
        .await
        .unwrap();
    assert_eq!((wrong.call_count, wrong.matching_calls), (0, 0));
    let huge = read(
        TokenCallQuery {
            offset: usize::MAX,
            ..Default::default()
        },
        OwnerFilter::Any,
    )
    .await
    .unwrap();
    assert!(huge.calls.is_empty());
    assert_eq!(huge.matching_calls, 57);
}

#[tokio::test]
async fn token_investigation_filters_before_paging_and_keeps_native_context() {
    use ai_memory_core::{TokenCallContext, TokenToolRef, TokenToolRelation};
    use ai_memory_store::{TokenCallOrder, TokenCallQuery};
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace("team").await.unwrap();
    let project = store
        .writer
        .get_or_create_project(ws, "app", None)
        .await
        .unwrap();
    let foreign_project = store
        .writer
        .get_or_create_project(ws, "private", None)
        .await
        .unwrap();
    let sid = session(&store, ws, project, AgentKind::Codex).await;
    let claude = session(&store, ws, project, AgentKind::ClaudeCode).await;
    let foreign = session(&store, ws, foreign_project, AgentKind::Codex).await;
    let make = |n: usize| {
        let mut c = call(10, 2);
        c.call_id = format!("response-{n}");
        c.model = Some("exact-model".into());
        c.context = Some(TokenCallContext {
            turn_id: Some("turn-17".into()),
            response_id: Some(format!("native-{n}")),
            recorded_at: Some("2026-10-01T12:00:00Z".into()),
            tools: vec![TokenToolRef {
                id: "tool-call-1".into(),
                name: "read_file".into(),
                relation: TokenToolRelation::Turn,
                is_error: Some(true),
            }],
        });
        c
    };
    store
        .writer
        .upsert_token_calls(sid, (0..57).map(make).collect())
        .await
        .unwrap();
    store
        .writer
        .upsert_token_calls(claude, vec![make(100)])
        .await
        .unwrap();
    store
        .writer
        .upsert_token_calls(foreign, vec![make(200)])
        .await
        .unwrap();
    let query = TokenCallQuery {
        model: Some("exact-model".into()),
        agent: Some(AgentKind::Codex),
        session_id: Some(sid),
        from_us: Some(
            "2026-10-01T12:00:00Z"
                .parse::<jiff::Timestamp>()
                .unwrap()
                .as_microsecond(),
        ),
        until_us: Some(
            "2026-10-01T12:00:01Z"
                .parse::<jiff::Timestamp>()
                .unwrap()
                .as_microsecond(),
        ),
        turn_id: Some("turn-17".into()),
        tool: Some("read_file".into()),
        order: TokenCallOrder::Recent,
        ..Default::default()
    };
    let first = store
        .reader
        .token_usage_report_page(ws, project, OwnerFilter::Any, None, query.clone())
        .await
        .unwrap();
    assert_eq!(
        (
            first.call_count,
            first.matching_calls,
            first.calls.len(),
            first.matching_tokens
        ),
        (58, 57, 50, 57 * 42)
    );
    let mut second_query = query.clone();
    second_query.offset = 50;
    let second = store
        .reader
        .token_usage_report_page(ws, project, OwnerFilter::Any, None, second_query)
        .await
        .unwrap();
    assert_eq!(second.calls.len(), 7);
    let ids = first
        .calls
        .iter()
        .chain(&second.calls)
        .map(|c| c.call_id.clone())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(ids.len(), 57, "Stable complete pagination");
    assert_eq!(first.calls[0].tools[0].name, "read_file");
    assert_eq!(first.calls[0].tools[0].is_error, Some(true));
    assert!(
        first.calls[0]
            .response_id
            .as_ref()
            .unwrap()
            .starts_with("native-")
    );
    for wrong in 0..8 {
        let mut q = query.clone();
        match wrong {
            0 => q.model = Some("exact".into()),
            1 => q.agent = Some(AgentKind::Other),
            2 => q.session_id = Some(foreign),
            3 => q.from_us = q.until_us,
            4 => q.until_us = q.from_us,
            5 => q.turn_id = Some("missing".into()),
            6 => q.tool = Some("read".into()),
            _ => q.session_id = Some(claude),
        };
        let r = store
            .reader
            .token_usage_report_page(ws, project, OwnerFilter::Any, None, q)
            .await
            .unwrap();
        assert_eq!(r.matching_calls, 0, "filter {wrong}");
        assert!(r.calls.is_empty());
    }
    let intersect = store
        .reader
        .token_usage_report_page(ws, project, OwnerFilter::Any, Some(claude), query)
        .await
        .unwrap();
    assert_eq!(
        intersect.matching_calls, 0,
        "Explicit session route must intersect the query"
    );
    let mut replay = make(0);
    replay.context = None;
    store
        .writer
        .upsert_token_calls(sid, vec![replay])
        .await
        .unwrap();
    let legacy = store
        .reader
        .session_token_usage(ws, project, sid, OwnerFilter::Any)
        .await
        .unwrap();
    assert!(
        legacy
            .calls
            .iter()
            .all(|c| c.turn_id.as_deref() == Some("turn-17") && c.tools.len() == 1),
        "Old-client replay must preserve provenance"
    );
}

#[tokio::test]
async fn token_context_rolls_back_with_response_counters_and_aggregates() {
    use ai_memory_core::{TokenCallContext, TokenToolRef, TokenToolRelation};
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace("team").await.unwrap();
    let project = store
        .writer
        .get_or_create_project(ws, "app", None)
        .await
        .unwrap();
    let sid = session(&store, ws, project, AgentKind::Codex).await;
    store
        .writer
        .upsert_token_calls(sid, vec![call(10, 2)])
        .await
        .unwrap();
    let conn = rusqlite::Connection::open(tmp.path().join("db/memory.sqlite")).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_context BEFORE INSERT ON token_call_tools WHEN NEW.tool_id='reject' BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
    let mut failure = call(40, 10);
    failure.call_id = "failed-response".into();
    failure.context = Some(TokenCallContext {
        turn_id: Some("failed-turn".into()),
        tools: vec![TokenToolRef {
            id: "reject".into(),
            name: "Read".into(),
            relation: TokenToolRelation::Requested,
            is_error: None,
        }],
        ..Default::default()
    });
    assert!(
        store
            .writer
            .upsert_token_calls(sid, vec![call(100, 20), failure])
            .await
            .is_err()
    );
    let report = store
        .reader
        .session_token_usage(ws, project, sid, OwnerFilter::Any)
        .await
        .unwrap();
    assert_eq!((report.call_count, report.input, report.output), (1, 10, 2));
    assert!(report.calls[0].turn_id.is_none());
    assert!(report.calls[0].tools.is_empty());
    let totals = store
        .reader
        .total_session_usage(ws, project, OwnerFilter::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(totals.input_tokens, 10);
    conn.execute_batch("DROP TRIGGER fail_context;").unwrap();
}
