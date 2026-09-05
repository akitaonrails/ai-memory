//! `session_counts_by_agent` — which agent CLIs produced a project's memory.
//!
//! The counts feed an operator dashboard, so the two properties that matter
//! are that the grouping is stable between calls and that one operator's
//! activity is not reported as another's on a shared server.

use ai_memory_core::{
    ActorContext, AgentKind, IdentityKey, NewSession, OwnerFilter, ProjectId, SessionId,
    WorkspaceId,
};
use ai_memory_store::Store;

fn operator(name: &str) -> String {
    IdentityKey::User(name.into()).storage_key()
}

fn filter_for(name: &str) -> OwnerFilter {
    OwnerFilter::for_actor_context(&ActorContext {
        user: Some(name.into()),
        ..ActorContext::default()
    })
}

async fn scope(store: &Store) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace("acme".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "app".to_string(), None)
        .await
        .unwrap();
    (ws, proj)
}

async fn session(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    agent: AgentKind,
    owner: Option<String>,
) {
    store
        .writer
        .begin_session(NewSession {
            id: SessionId::new(),
            workspace_id: ws,
            project_id: proj,
            agent_kind: agent,
            cwd: None,
            actor_user: owner,
        })
        .await
        .unwrap();
}

/// Grouping is by agent, ordered count-descending. The tiebreak is the agent
/// name, so two agents with equal counts keep a stable order across calls
/// rather than following SQLite's scan order.
#[tokio::test]
async fn counts_group_by_agent_and_order_deterministically() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    for _ in 0..3 {
        session(&store, ws, proj, AgentKind::ClaudeCode, None).await;
    }
    // `codex` and `cursor` tie at one session each.
    session(&store, ws, proj, AgentKind::Cursor, None).await;
    session(&store, ws, proj, AgentKind::Codex, None).await;

    let counts = store
        .reader
        .session_counts_by_agent(ws, proj, OwnerFilter::Any, None)
        .await
        .unwrap();

    let seen: Vec<(&str, u64)> = counts
        .iter()
        .map(|c| (c.agent.as_str(), c.sessions))
        .collect();
    assert_eq!(
        seen,
        vec![("claude-code", 3), ("codex", 1), ("cursor", 1)],
        "count desc, then agent name asc",
    );

    // Same query twice must not reorder the tied pair.
    let again = store
        .reader
        .session_counts_by_agent(ws, proj, OwnerFilter::Any, None)
        .await
        .unwrap();
    assert_eq!(counts, again);
}

/// The window is an inclusive lower bound on `started_at`. A cutoff in the
/// future excludes everything rather than silently ignoring the argument.
#[tokio::test]
async fn since_bounds_the_window() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    session(&store, ws, proj, AgentKind::ClaudeCode, None).await;

    let all = store
        .reader
        .session_counts_by_agent(ws, proj, OwnerFilter::Any, None)
        .await
        .unwrap();
    assert_eq!(all.len(), 1, "no bound counts the whole history");

    let future = jiff::Timestamp::now().as_microsecond() + 60_000_000;
    let none = store
        .reader
        .session_counts_by_agent(ws, proj, OwnerFilter::Any, Some(future))
        .await
        .unwrap();
    assert!(none.is_empty(), "a future cutoff excludes every session");
}

/// On a shared server the dashboard must not report a teammate's activity as
/// the caller's. Unowned (single-user / legacy) rows stay visible to everyone,
/// matching the `absent = shared` rule the rest of the ownership work uses.
#[tokio::test]
async fn counts_are_scoped_to_the_asking_operator() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    session(
        &store,
        ws,
        proj,
        AgentKind::ClaudeCode,
        Some(operator("alice")),
    )
    .await;
    session(&store, ws, proj, AgentKind::Cursor, Some(operator("bob"))).await;
    // Written before the server distinguished operators.
    session(&store, ws, proj, AgentKind::Codex, None).await;

    let alice = store
        .reader
        .session_counts_by_agent(ws, proj, filter_for("alice"), None)
        .await
        .unwrap();
    let alice_agents: Vec<&str> = alice.iter().map(|c| c.agent.as_str()).collect();
    assert_eq!(
        alice_agents,
        vec!["claude-code", "codex"],
        "own rows plus shared rows, never bob's",
    );

    let everyone = store
        .reader
        .session_counts_by_agent(ws, proj, OwnerFilter::Any, None)
        .await
        .unwrap();
    assert_eq!(
        everyone.len(),
        3,
        "the recovery switch reports all operators"
    );
}

/// A caller the server cannot name — no bearer identity, no proxy-asserted
/// user — resolves to `Unattributed` and must see only the shared rows.
/// This is the shape a single-user install has for every request, so it is
/// also the path that must keep working when nobody configured multi-user.
#[tokio::test]
async fn an_unidentified_caller_sees_only_shared_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    session(
        &store,
        ws,
        proj,
        AgentKind::ClaudeCode,
        Some(operator("alice")),
    )
    .await;
    session(&store, ws, proj, AgentKind::Cursor, None).await;

    let anonymous = OwnerFilter::for_actor_context(&ActorContext::anonymous());
    assert_eq!(
        anonymous,
        OwnerFilter::Unattributed,
        "a caller with no identity must not resolve to a named owner",
    );

    let counts = store
        .reader
        .session_counts_by_agent(ws, proj, anonymous, None)
        .await
        .unwrap();
    let agents: Vec<&str> = counts.iter().map(|c| c.agent.as_str()).collect();
    assert_eq!(
        agents,
        vec!["cursor"],
        "shared rows only — alice's session is not this caller's to count",
    );
}
