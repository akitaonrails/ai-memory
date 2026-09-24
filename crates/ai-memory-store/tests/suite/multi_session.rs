//! Integration tests for the multi-session and multi-user scenarios.
//!
//! These are the guarantees a team and a parallel-harness workflow depend on,
//! and they are deliberately at integration level: the unit tests around them
//! exercise one session at a time, which is exactly the shape that cannot see
//! a collaboration or concurrency defect.
//!
//! Two halves, and they pull in opposite directions:
//!
//! * **Shared** — knowledge written by one operator must be readable by
//!   another in the same project. Pages carry an `author_id` for attribution,
//!   and it must never become a filter.
//! * **Owned** — a handoff is a baton. Exactly one session may take it, and a
//!   second attempt must not be able to steal it.
//!
//! Anything that makes pages owner-filtered, or handoffs stealable, breaks a
//! core capability rather than a detail.

use ai_memory_core::{
    ActorContext, AgentKind, HandoffAcceptance, IdentityKey, NewHandoff, NewPage, NewSession,
    NewUser, OwnerFilter, PagePath, ProjectId, SessionId, Tier, UserRole, WorkspaceId, owner_stamp,
};
use ai_memory_store::Store;

fn operator(name: &str) -> String {
    IdentityKey::User(name.into()).storage_key()
}

async fn scope(store: &Store) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace("acme".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "shared-app".to_string(), None)
        .await
        .unwrap();
    (ws, proj)
}

/// Open a real session row. `accept_handoff` requires the receiver to exist —
/// a guard worth keeping, so the tests satisfy it rather than route around it.
async fn open_session(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    agent_kind: AgentKind,
) -> SessionId {
    let id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id,
            workspace_id: ws,
            project_id: proj,
            agent_kind,
            cwd: Some("/repo".into()),
            actor_user: None,
        })
        .await
        .unwrap();
    id
}

fn page(ws: WorkspaceId, proj: ProjectId, path: &str, title: &str, body: &str) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: title.into(),
        body: body.into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

/// The collaboration guarantee, and the reason a team can use one server:
/// what Alice writes, Carol reads.
///
/// `pages.author_id` exists for attribution and must never become a read
/// filter. If it ever does, this fails — and a team silently stops sharing
/// knowledge while every single-user test still passes.
#[tokio::test]
async fn one_operators_page_is_readable_by_another_in_the_same_project() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "decisions/0001.md",
            "Chose SQLite",
            "We picked SQLite for the derived index.",
        ))
        .await
        .unwrap();

    // Carol's read of the same project: no owner coordinate involved.
    let hits = store
        .reader
        .search_pages("SQLite".to_string(), 10)
        .await
        .unwrap();

    assert!(
        hits.iter().any(|h| h.path.as_str() == "decisions/0001.md"),
        "a page written in this project must be visible to any operator \
         reading it; got {:?}",
        hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>()
    );

    // …and readable in full, not just rankable.
    let body = store
        .reader
        .page_body_by_ids(ws, proj, "decisions/0001.md")
        .await
        .unwrap()
        .expect("the page resolves by path for any reader");
    assert!(body.body.contains("We picked SQLite"));
}

/// The stronger form of the collaboration guarantee: the existing sibling
/// test writes with `author_id: None`, so it cannot tell an "authored pages
/// are private" regression from a genuine bug — a filter keyed on the
/// caller's identity would happily let a NULL-authored page through. This
/// stamps a real, non-null `author_id` (operator A) and asserts operator B —
/// a *different* identity, reading with no owner coordinate at all, exactly
/// as `search_pages_for_project` and `page_body_by_ids` are shaped — still
/// sees the page in full, through both the search path and the direct-body
/// path. `pages.author_id` is attribution, never a read filter.
#[tokio::test]
async fn an_authored_page_is_readable_by_a_different_operator_via_search_and_body() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let operator_a = store
        .writer
        .create_human_user(
            NewUser {
                username: "operator-a".into(),
                name: Some("Operator A".into()),
                email: Some("operator-a@example.com".into()),
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();

    store
        .writer
        .upsert_page(NewPage {
            author_id: Some(operator_a),
            ..page(
                ws,
                proj,
                "decisions/0002.md",
                "Chose SQLite Again",
                "We picked SQLite for the derived index, authored by operator A.",
            )
        })
        .await
        .unwrap();

    // Operator B's read: no owner coordinate passed anywhere, because none of
    // these signatures accept one — that absence IS the invariant.
    let hits = store
        .reader
        .search_pages_for_project(ws, proj, "SQLite Again".to_string(), 10, None)
        .await
        .unwrap();
    assert!(
        hits.iter().any(|h| h.path.as_str() == "decisions/0002.md"),
        "an authored page must be visible to a different operator's search; \
         got {:?}",
        hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>()
    );

    let body = store
        .reader
        .page_body_by_ids(ws, proj, "decisions/0002.md")
        .await
        .unwrap()
        .expect("a different operator can still resolve the page by path");
    assert!(body.body.contains("authored by operator A"));
}

/// Two harnesses editing the same page keep both versions.
///
/// The latest write wins the `is_latest` flag — there is no merge, and none is
/// claimed — but the superseded version stays reachable through the chain.
/// "Last write wins" must never mean "the other version is gone".
#[tokio::test]
async fn concurrent_writes_to_one_path_supersede_rather_than_destroy() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let first = store
        .writer
        .upsert_page(page(ws, proj, "notes/shared.md", "Shared", "alice's text"))
        .await
        .unwrap();
    let second = store
        .writer
        .upsert_page(page(ws, proj, "notes/shared.md", "Shared", "carol's text"))
        .await
        .unwrap();

    assert_ne!(first, second, "a divergent write creates a new version");

    let latest = store
        .reader
        .page_body_by_ids(ws, proj, "notes/shared.md")
        .await
        .unwrap()
        .expect("the path still resolves");
    assert!(
        latest.body.contains("carol's text"),
        "the later write is the latest version"
    );

    let latest_id = store
        .reader
        .latest_page_id_by_ids(ws, proj, "notes/shared.md".to_string())
        .await
        .unwrap()
        .expect("a latest version exists");
    assert_eq!(latest_id, second, "the later write holds is_latest");

    // The overwritten version is still a row, reachable by its own id.
    let earlier_survives = store
        .reader
        .with_conn(move |conn| {
            let body: String = conn.query_row(
                "SELECT body FROM pages WHERE id = ?1",
                rusqlite::params![&first.as_bytes()[..]],
                |r| r.get(0),
            )?;
            Ok(body)
        })
        .await
        .unwrap();
    assert!(
        earlier_survives.contains("alice's text"),
        "the overwritten version must survive in the supersession chain, \
         not be destroyed"
    );
}

/// Re-writing identical content must not churn a new version.
///
/// Two harnesses syncing the same file would otherwise manufacture a version
/// per pass and bloat the chain with nothing to show for it.
#[tokio::test]
async fn an_identical_rewrite_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let first = store
        .writer
        .upsert_page(page(ws, proj, "notes/same.md", "Same", "identical body"))
        .await
        .unwrap();
    let again = store
        .writer
        .upsert_page(page(ws, proj, "notes/same.md", "Same", "identical body"))
        .await
        .unwrap();

    assert_eq!(
        first, again,
        "identical content must return the same version, not create one"
    );
}

/// A handoff is a baton: exactly one session takes it.
///
/// The pre-existing unit test asserted only that a second accept does not
/// *error*, which would still pass if the second accept overwrote
/// `accepted_by`. This asserts the property that actually matters — the first
/// accepter keeps it.
#[tokio::test]
async fn a_second_accept_cannot_steal_an_accepted_handoff() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let id = store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            from_session_id: None,
            summary: "pick this up".into(),
            next_steps: Vec::new(),
            open_questions: Vec::new(),
            files_touched: Vec::new(),
            cwd: None,
            owner_user: None,
        })
        .await
        .unwrap();

    let accept = |agent: AgentKind, session: SessionId| HandoffAcceptance {
        handoff_id: id,
        workspace_id: ws,
        project_id: proj,
        accepting_agent: agent,
        accepting_session: Some(session),
        accepting_user: None,
        owner_filter: OwnerFilter::Any,
        receiving_cwd: None,
    };

    let winner = open_session(&store, ws, proj, AgentKind::Codex).await;
    let loser = open_session(&store, ws, proj, AgentKind::ClaudeCode).await;

    let first = store
        .writer
        .accept_handoff(accept(AgentKind::Codex, winner))
        .await
        .unwrap();
    assert!(first, "the first accept claims the baton");

    let second = store
        .writer
        .accept_handoff(accept(AgentKind::ClaudeCode, loser))
        .await
        .unwrap();
    assert!(
        !second,
        "a second accept must report that it claimed nothing"
    );

    let handoff_bytes = id.as_bytes().to_vec();
    let (accepted_by, accepted_session): (Option<String>, Option<Vec<u8>>) = store
        .reader
        .with_conn(move |conn| {
            Ok(conn.query_row(
                "SELECT accepted_by, accepted_by_session FROM handoffs WHERE id = ?1",
                rusqlite::params![handoff_bytes],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await
        .unwrap();

    assert_eq!(
        accepted_by.as_deref(),
        Some("codex"),
        "the original accepter must still own the handoff"
    );
    assert_eq!(
        accepted_session.as_deref(),
        Some(&winner.as_bytes()[..]),
        "and the winning session must not have been overwritten by the loser"
    );
}

/// Ownership still applies to batons even though pages are shared: the two
/// halves of the model must not collapse into each other.
#[tokio::test]
async fn an_owned_handoff_stays_with_its_owner_while_pages_stay_shared() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let id = store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            from_session_id: None,
            summary: "alice's baton".into(),
            next_steps: Vec::new(),
            open_questions: Vec::new(),
            files_touched: Vec::new(),
            cwd: None,
            owner_user: owner_stamp(Some(&IdentityKey::User("alice".into())), true),
        })
        .await
        .unwrap();

    let carol = OwnerFilter::for_actor_context(&ActorContext {
        user: Some("carol".into()),
        ..ActorContext::default()
    });

    let stolen = store
        .writer
        .accept_handoff(HandoffAcceptance {
            handoff_id: id,
            workspace_id: ws,
            project_id: proj,
            accepting_agent: AgentKind::Codex,
            accepting_session: Some(open_session(&store, ws, proj, AgentKind::Codex).await),
            accepting_user: Some(operator("carol")),
            owner_filter: carol,
            receiving_cwd: None,
        })
        .await
        .unwrap();

    assert!(
        !stolen,
        "carol must not be able to accept a baton owned by {}",
        operator("alice")
    );
}

/// Grok reuses one session id across a SessionEnd→restart, so
/// `accept_handoff` must reopen an already-ended receiver session instead of
/// rejecting it (#840) — but only *after* the exactly-once claim guard, so the
/// resurrection can never become a way to steal an already-taken baton.
#[tokio::test]
async fn accept_reopens_an_ended_receiver_session_but_keeps_claim_once() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let id = store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_agent: AgentKind::Grok,
            to_agent: None,
            from_session_id: None,
            summary: "resume after restart".into(),
            next_steps: Vec::new(),
            open_questions: Vec::new(),
            files_touched: Vec::new(),
            cwd: None,
            owner_user: None,
        })
        .await
        .unwrap();

    let accept = |session: SessionId| HandoffAcceptance {
        handoff_id: id,
        workspace_id: ws,
        project_id: proj,
        accepting_agent: AgentKind::Grok,
        accepting_session: Some(session),
        accepting_user: None,
        owner_filter: OwnerFilter::Any,
        receiving_cwd: None,
    };

    // The same Grok session id: opened, then ended (SessionEnd), then reused
    // when the conversation restarts and calls its first tool.
    let grok = open_session(&store, ws, proj, AgentKind::Grok).await;
    store.writer.end_session(grok, None).await.unwrap();

    let claimed = store.writer.accept_handoff(accept(grok)).await.unwrap();
    assert!(
        claimed,
        "an ended session that reuses its id must be able to accept the handoff"
    );

    // The receiver row was reopened (ended_at cleared), not left a corpse.
    let grok_bytes = grok.as_bytes().to_vec();
    let ended_at: Option<i64> = store
        .reader
        .with_conn(move |conn| {
            Ok(conn.query_row(
                "SELECT ended_at FROM sessions WHERE id = ?1",
                rusqlite::params![grok_bytes],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(
        ended_at.is_none(),
        "accepting a handoff must reopen the ended receiver session"
    );

    // Claim-once still holds: a *different* session cannot steal the baton the
    // reopened session already took, even though the loser is wide open.
    let loser = open_session(&store, ws, proj, AgentKind::Codex).await;
    let stolen = store
        .writer
        .accept_handoff(HandoffAcceptance {
            handoff_id: id,
            workspace_id: ws,
            project_id: proj,
            accepting_agent: AgentKind::Codex,
            accepting_session: Some(loser),
            accepting_user: None,
            owner_filter: OwnerFilter::Any,
            receiving_cwd: None,
        })
        .await
        .unwrap();
    assert!(
        !stolen,
        "the resurrection path must not let a second session steal an accepted baton"
    );
}
