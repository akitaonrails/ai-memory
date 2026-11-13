//! `page_sources` (V108, #2164): which observations a page version was
//! compiled from.
//!
//! Invariants under test:
//! - **Exact.** A page lists the observations its write named, never the rest
//!   of the session.
//! - **In scope.** A write cannot make a page name an observation of another
//!   project, another workspace or another session (adversarial, with a
//!   same-scope control). A whole-session citation takes only what the session
//!   has in the page's own project.
//! - **Per version.** A new version starts from its own compile; the
//!   superseded version keeps its rows; an unchanged version accumulates.
//! - **Atomic.** A failed batch leaves no row behind.
//! - **Owner filter.** Another operator's observation is hidden; the page and
//!   its unattributed sources are shared (invariant #16).
//! - **Lifecycle.** Rows follow their page through move-project,
//!   merge-project and move-session, cascade with a deleted version, and
//!   outlive a pruned observation as an id.

use ai_memory_core::{
    AgentKind, NewObservation, NewPage, NewSession, ObservationId, ObservationKind, OwnerFilter,
    PageEvidence, PageEvidenceKind, PageId, PagePath, ProjectId, Sanitized, Sanitizer, SessionId,
    SourceObservations, Tier, WorkspaceId,
};
use ai_memory_store::{MergeProjectPage, PagesMode, Store, StoreError};
use rusqlite::{Connection, params};

struct Fixture {
    _tmp: tempfile::TempDir,
    store: Store,
    ws: WorkspaceId,
    app: ProjectId,
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_owned())
        .await
        .unwrap();
    let app = project(&store, ws, "app").await;
    Fixture {
        _tmp: tmp,
        store,
        ws,
        app,
    }
}

async fn project(store: &Store, ws: WorkspaceId, name: &str) -> ProjectId {
    store
        .writer
        .get_or_create_project(ws, name.to_owned(), None)
        .await
        .unwrap()
}

async fn session(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    owner: Option<&str>,
) -> SessionId {
    let id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: owner.map(str::to_owned),
            origin: None,
        })
        .await
        .unwrap();
    id
}

/// Capture one observation for `session`, stamped into `(ws, proj)`. The
/// project is a parameter because a session's observations can land outside
/// the session row's own project.
async fn observe(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    session: SessionId,
    kind: ObservationKind,
    at: i64,
) -> ObservationId {
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                occurred_at: Some(at),
                session_id: session,
                workspace_id: ws,
                project_id: proj,
                kind,
                extension: None,
                source_event: None,
                title: kind.as_str().into(),
                body: format!("body at {at}"),
                importance: 5,
                file_path: None,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap()
}

fn page(
    ws: WorkspaceId,
    proj: ProjectId,
    path: &str,
    body: &str,
    evidence: Vec<PageEvidence>,
) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: path.to_owned(),
        body: body.into(),
        tier: Tier::Episodic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence,
    }
}

fn cites(session: SessionId, observations: SourceObservations) -> Vec<PageEvidence> {
    vec![PageEvidence {
        kind: PageEvidenceKind::Session,
        source_id: session.to_string(),
        observations,
    }]
}

fn listed(session: SessionId, ids: &[ObservationId]) -> Vec<PageEvidence> {
    cites(session, SourceObservations::Listed(ids.to_vec()))
}

/// The source ids the reader returns for the current version at `path`.
async fn sources(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    path: &str,
    owner: OwnerFilter,
) -> Vec<ObservationId> {
    let read = store
        .reader
        .page_compiled_sources(ws, proj, path.to_owned(), owner, 0, 500)
        .await
        .unwrap()
        .expect("the page exists");
    assert_eq!(
        read.source_count,
        read.sources.len(),
        "one window holds all"
    );
    read.sources.iter().map(|s| s.observation_id).collect()
}

fn sorted(mut ids: Vec<ObservationId>) -> Vec<ObservationId> {
    ids.sort_by_key(ToString::to_string);
    ids
}

/// Rows recorded against one page version, with the coordinate they carry.
fn raw_rows(store: &Store, page_id: PageId) -> Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let conn = Connection::open(store.db_path()).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT workspace_id, project_id, observation_id FROM page_sources \
             WHERE page_id = ?1 ORDER BY observation_id",
        )
        .unwrap();
    stmt.query_map(params![page_id.as_bytes()], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

fn total_rows(store: &Store) -> i64 {
    Connection::open(store.db_path())
        .unwrap()
        .query_row("SELECT COUNT(*) FROM page_sources", [], |r| r.get(0))
        .unwrap()
}

#[tokio::test]
async fn a_page_lists_exactly_the_observations_its_write_named() {
    let f = fixture().await;
    let sid = session(&f.store, f.ws, f.app, None).await;
    let prompt = observe(&f.store, f.ws, f.app, sid, ObservationKind::UserPrompt, 10).await;
    let tool = observe(&f.store, f.ws, f.app, sid, ObservationKind::PostToolUse, 20).await;
    let left_out = observe(&f.store, f.ws, f.app, sid, ObservationKind::PostToolUse, 30).await;

    f.store
        .writer
        .upsert_page(page(
            f.ws,
            f.app,
            "sessions/a.md",
            "compiled",
            listed(sid, &[tool, prompt]),
        ))
        .await
        .unwrap();

    let read = f
        .store
        .reader
        .page_compiled_sources(
            f.ws,
            f.app,
            "sessions/a.md".into(),
            OwnerFilter::Any,
            0,
            500,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.source_count, 2);
    // Oldest observation first, each with what is still stored about it.
    let listed: Vec<_> = read
        .sources
        .iter()
        .map(|s| {
            let stored = s.observation.as_ref().expect("still stored");
            (s.observation_id, stored.kind, stored.session_id)
        })
        .collect();
    assert_eq!(
        listed,
        vec![
            (prompt, ObservationKind::UserPrompt, sid),
            (tool, ObservationKind::PostToolUse, sid),
        ],
        "the session's third observation was not in the compile: {left_out}"
    );

    // The window is honoured and never changes the visible total.
    let second = f
        .store
        .reader
        .page_compiled_sources(f.ws, f.app, "sessions/a.md".into(), OwnerFilter::Any, 1, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.source_count, 2);
    assert_eq!(second.sources.len(), 1);
    assert_eq!(second.sources[0].observation_id, tool);

    // A page that names no sources reads as an empty list; a missing page
    // reads as `None`.
    f.store
        .writer
        .upsert_page(page(f.ws, f.app, "notes/hand.md", "by hand", Vec::new()))
        .await
        .unwrap();
    assert!(
        sources(&f.store, f.ws, f.app, "notes/hand.md", OwnerFilter::Any)
            .await
            .is_empty()
    );
    assert!(
        f.store
            .reader
            .page_compiled_sources(f.ws, f.app, "notes/none.md".into(), OwnerFilter::Any, 0, 10)
            .await
            .unwrap()
            .is_none()
    );
}

/// Adversarial: observation ids arrive inside the write, so each foreign one
/// must be refused. Removing the session or the scope predicate from
/// `insert_page_sources_in_tx` fails this test.
#[tokio::test]
async fn a_write_cannot_name_an_observation_outside_its_project_or_session() {
    let f = fixture().await;
    let sibling = project(&f.store, f.ws, "sibling").await;
    let other_ws = f
        .store
        .writer
        .get_or_create_workspace("other".to_owned())
        .await
        .unwrap();
    let far = project(&f.store, other_ws, "app").await;

    let sid = session(&f.store, f.ws, f.app, None).await;
    let own = observe(&f.store, f.ws, f.app, sid, ObservationKind::UserPrompt, 10).await;
    // The same session also left an observation in a sibling project.
    let scattered = observe(
        &f.store,
        f.ws,
        sibling,
        sid,
        ObservationKind::PostToolUse,
        20,
    )
    .await;
    let neighbour = session(&f.store, f.ws, f.app, None).await;
    let other_session = observe(
        &f.store,
        f.ws,
        f.app,
        neighbour,
        ObservationKind::UserPrompt,
        30,
    )
    .await;
    let sibling_session = session(&f.store, f.ws, sibling, None).await;
    let foreign_project = observe(
        &f.store,
        f.ws,
        sibling,
        sibling_session,
        ObservationKind::PostToolUse,
        40,
    )
    .await;
    let far_session = session(&f.store, other_ws, far, None).await;
    let foreign_workspace = observe(
        &f.store,
        other_ws,
        far,
        far_session,
        ObservationKind::PostToolUse,
        50,
    )
    .await;

    f.store
        .writer
        .upsert_page(page(
            f.ws,
            f.app,
            "sessions/listed.md",
            "listed",
            listed(
                sid,
                &[
                    own,
                    scattered,
                    other_session,
                    foreign_project,
                    foreign_workspace,
                    ObservationId::new(),
                ],
            ),
        ))
        .await
        .unwrap();
    assert_eq!(
        sources(
            &f.store,
            f.ws,
            f.app,
            "sessions/listed.md",
            OwnerFilter::Any
        )
        .await,
        vec![own],
        "only the cited session's observation in the page's project is kept"
    );

    // Citing the foreign session itself does not help: its observations are
    // not in this page's project.
    f.store
        .writer
        .upsert_page(page(
            f.ws,
            f.app,
            "sessions/foreign.md",
            "foreign",
            listed(sibling_session, &[foreign_project]),
        ))
        .await
        .unwrap();
    f.store
        .writer
        .upsert_page(page(
            f.ws,
            f.app,
            "sessions/foreign-whole.md",
            "foreign whole",
            cites(far_session, SourceObservations::WholeSession),
        ))
        .await
        .unwrap();
    for path in ["sessions/foreign.md", "sessions/foreign-whole.md"] {
        assert!(
            sources(&f.store, f.ws, f.app, path, OwnerFilter::Any)
                .await
                .is_empty(),
            "{path}"
        );
    }

    // A whole-session citation takes the session's rows in this project and
    // leaves the one that landed in the sibling.
    f.store
        .writer
        .upsert_page(page(
            f.ws,
            f.app,
            "sessions/whole.md",
            "whole",
            cites(sid, SourceObservations::WholeSession),
        ))
        .await
        .unwrap();
    assert_eq!(
        sources(&f.store, f.ws, f.app, "sessions/whole.md", OwnerFilter::Any).await,
        vec![own]
    );

    // No row anywhere carries a coordinate other than the page's.
    let conn = Connection::open(f.store.db_path()).unwrap();
    let stray: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM page_sources WHERE workspace_id <> ?1 OR project_id <> ?2",
            params![f.ws.as_bytes(), f.app.as_bytes()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stray, 0);
    assert_eq!(total_rows(&f.store), 2);

    // A session citation that names no observations, and a non-session
    // citation, record nothing.
    f.store
        .writer
        .upsert_page(page(
            f.ws,
            f.app,
            "sessions/unrecorded.md",
            "unrecorded",
            vec![
                PageEvidence {
                    kind: PageEvidenceKind::Session,
                    source_id: sid.to_string(),
                    observations: SourceObservations::Unrecorded,
                },
                PageEvidence {
                    kind: PageEvidenceKind::Reconsolidation,
                    source_id: sid.to_string(),
                    observations: SourceObservations::WholeSession,
                },
            ],
        ))
        .await
        .unwrap();
    assert_eq!(total_rows(&f.store), 2);
}

#[tokio::test]
async fn sources_are_rederived_per_version_and_accumulate_on_an_unchanged_one() {
    let f = fixture().await;
    let sid = session(&f.store, f.ws, f.app, None).await;
    let a = observe(&f.store, f.ws, f.app, sid, ObservationKind::UserPrompt, 10).await;
    let b = observe(&f.store, f.ws, f.app, sid, ObservationKind::PostToolUse, 20).await;
    let c = observe(&f.store, f.ws, f.app, sid, ObservationKind::Stop, 30).await;
    let path = "sessions/chain.md";
    let latest = |owner| sources(&f.store, f.ws, f.app, path, owner);

    let v1 = f
        .store
        .writer
        .upsert_page(page(f.ws, f.app, path, "v1", listed(sid, &[a])))
        .await
        .unwrap();
    assert_eq!(latest(OwnerFilter::Any).await, vec![a]);

    // A recompile that changes the body starts from its own sources.
    let v2 = f
        .store
        .writer
        .upsert_page(page(f.ws, f.app, path, "v2", listed(sid, &[b])))
        .await
        .unwrap();
    assert_ne!(v1, v2);
    assert_eq!(latest(OwnerFilter::Any).await, vec![b]);
    // The superseded version keeps what it was compiled from.
    assert_eq!(
        raw_rows(&f.store, v1)
            .into_iter()
            .map(|row| row.2)
            .collect::<Vec<_>>(),
        vec![a.as_bytes().to_vec()]
    );

    // The same body from another compile is the same version: it now lists
    // both compiles, and repeating one adds nothing.
    for _ in 0..2 {
        let same = f
            .store
            .writer
            .upsert_page(page(f.ws, f.app, path, "v2", listed(sid, &[b, c])))
            .await
            .unwrap();
        assert_eq!(same, v2);
    }
    assert_eq!(latest(OwnerFilter::Any).await, vec![b, c]);

    // A new version written without a citation records nothing: unknown,
    // not inherited from the version it replaces.
    let v3 = f
        .store
        .writer
        .upsert_page(page(f.ws, f.app, path, "v3 by hand", Vec::new()))
        .await
        .unwrap();
    assert!(latest(OwnerFilter::Any).await.is_empty());
    assert!(raw_rows(&f.store, v3).is_empty());
    assert_eq!(raw_rows(&f.store, v2).len(), 2);
}

/// A batch is one transaction: when a later page fails, the earlier page and
/// its sources are gone together.
#[tokio::test]
async fn a_failed_batch_rolls_its_sources_back_with_its_pages() {
    let f = fixture().await;
    let sid = session(&f.store, f.ws, f.app, None).await;
    let a = observe(&f.store, f.ws, f.app, sid, ObservationKind::UserPrompt, 10).await;
    f.store
        .writer
        .upsert_page(page(f.ws, f.app, "notes/Taken.md", "live", Vec::new()))
        .await
        .unwrap();

    let failed = f
        .store
        .writer
        .upsert_pages_batch(vec![
            page(f.ws, f.app, "sessions/first.md", "first", listed(sid, &[a])),
            // Collides with the live `notes/Taken.md` on a case-folding
            // filesystem, which the store refuses.
            page(f.ws, f.app, "notes/taken.md", "second", listed(sid, &[a])),
        ])
        .await;
    assert!(
        matches!(failed, Err(StoreError::PagePathCollides { .. })),
        "{failed:?}"
    );
    assert_eq!(total_rows(&f.store), 0, "no source outlives the rollback");
    assert!(
        f.store
            .reader
            .page_compiled_sources(
                f.ws,
                f.app,
                "sessions/first.md".into(),
                OwnerFilter::Any,
                0,
                10
            )
            .await
            .unwrap()
            .is_none(),
        "the first page rolled back too"
    );

    // Control: the same first page alone commits with its source.
    f.store
        .writer
        .upsert_pages_batch(vec![page(
            f.ws,
            f.app,
            "sessions/first.md",
            "first",
            listed(sid, &[a]),
        )])
        .await
        .unwrap();
    assert_eq!(
        sources(&f.store, f.ws, f.app, "sessions/first.md", OwnerFilter::Any).await,
        vec![a]
    );
}

/// The page is shared; an observation id of another operator's session is
/// not. Dropping the owner clause from `page_compiled_sources` fails this.
#[tokio::test]
async fn another_operators_observation_is_hidden_while_the_page_stays_shared() {
    let f = fixture().await;
    let alice = "user:alice";
    let owned = session(&f.store, f.ws, f.app, Some(alice)).await;
    let shared = session(&f.store, f.ws, f.app, None).await;
    let private = observe(
        &f.store,
        f.ws,
        f.app,
        owned,
        ObservationKind::UserPrompt,
        10,
    )
    .await;
    let open = observe(
        &f.store,
        f.ws,
        f.app,
        shared,
        ObservationKind::UserPrompt,
        20,
    )
    .await;
    let path = "notes/both.md";
    let mut evidence = listed(owned, &[private]);
    evidence.extend(listed(shared, &[open]));
    f.store
        .writer
        .upsert_page(page(f.ws, f.app, path, "both", evidence))
        .await
        .unwrap();

    let seen = |owner| sources(&f.store, f.ws, f.app, path, owner);
    assert_eq!(seen(OwnerFilter::Any).await, vec![private, open]);
    assert_eq!(
        seen(OwnerFilter::User(alice.into())).await,
        vec![private, open]
    );
    assert_eq!(seen(OwnerFilter::User("user:bob".into())).await, vec![open]);
    assert_eq!(seen(OwnerFilter::Unattributed).await, vec![open]);
}

/// Retention prunes observations; the page keeps naming what it was compiled
/// from, without the detail that is no longer stored.
#[tokio::test]
async fn a_pruned_observation_stays_listed_by_id() {
    let f = fixture().await;
    let sid = session(&f.store, f.ws, f.app, None).await;
    let kept = observe(&f.store, f.ws, f.app, sid, ObservationKind::UserPrompt, 10).await;
    let pruned = observe(&f.store, f.ws, f.app, sid, ObservationKind::PostToolUse, 20).await;
    f.store
        .writer
        .upsert_page(page(
            f.ws,
            f.app,
            "sessions/p.md",
            "compiled",
            listed(sid, &[kept, pruned]),
        ))
        .await
        .unwrap();
    // What the retention prune does to one row.
    Connection::open(f.store.db_path())
        .unwrap()
        .execute(
            "DELETE FROM observations WHERE id = ?1",
            params![pruned.as_bytes()],
        )
        .unwrap();

    // Even a filtered caller sees it: there is no session left to own it.
    let read = f
        .store
        .reader
        .page_compiled_sources(
            f.ws,
            f.app,
            "sessions/p.md".into(),
            OwnerFilter::Unattributed,
            0,
            10,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.source_count, 2);
    assert_eq!(read.sources[0].observation_id, kept);
    assert!(read.sources[0].observation.is_some());
    assert_eq!(read.sources[1].observation_id, pruned);
    assert_eq!(read.sources[1].observation, None);
}

/// A session can be moved to another project after a page here cited it. The
/// page keeps the id; the observation's kind and session now belong to the
/// other project and are not read through this one. Dropping the coordinate
/// from the reader's observation join fails this test.
#[tokio::test]
async fn an_observation_that_left_the_project_is_listed_by_id_only() {
    let f = fixture().await;
    let target = project(&f.store, f.ws, "target").await;
    let sid = session(&f.store, f.ws, f.app, None).await;
    let a = observe(&f.store, f.ws, f.app, sid, ObservationKind::UserPrompt, 10).await;
    f.store.writer.end_session(sid, None).await.unwrap();
    f.store
        .writer
        .upsert_page(page(
            f.ws,
            f.app,
            "notes/stays.md",
            "stays",
            listed(sid, &[a]),
        ))
        .await
        .unwrap();
    let read = || async {
        f.store
            .reader
            .page_compiled_sources(
                f.ws,
                f.app,
                "notes/stays.md".into(),
                OwnerFilter::Any,
                0,
                10,
            )
            .await
            .unwrap()
            .unwrap()
    };
    assert!(read().await.sources[0].observation.is_some(), "control");

    f.store
        .writer
        .move_session(sid, f.ws, target, PagesMode::Move, None, true)
        .await
        .unwrap();
    let after = read().await;
    assert_eq!(after.source_count, 1);
    assert_eq!(after.sources[0].observation_id, a);
    assert_eq!(after.sources[0].observation, None);
}

/// Rows repeat their page's coordinate, so every operation that re-stamps a
/// page must take them along. Each arm fails when its `page_sources` update
/// is removed from the operation.
#[tokio::test]
async fn sources_follow_their_page_through_move_and_merge() {
    // move-project: the project changes workspace.
    {
        let f = fixture().await;
        let sid = session(&f.store, f.ws, f.app, None).await;
        let a = observe(&f.store, f.ws, f.app, sid, ObservationKind::UserPrompt, 10).await;
        let id = f
            .store
            .writer
            .upsert_page(page(f.ws, f.app, "notes/m.md", "m", listed(sid, &[a])))
            .await
            .unwrap();
        let to = f
            .store
            .writer
            .get_or_create_workspace("elsewhere".to_owned())
            .await
            .unwrap();
        f.store
            .writer
            .move_project_workspace(f.app, f.ws, to)
            .await
            .unwrap();
        assert_eq!(
            raw_rows(&f.store, id),
            vec![(
                to.as_bytes().to_vec(),
                f.app.as_bytes().to_vec(),
                a.as_bytes().to_vec()
            )]
        );
        assert_eq!(
            sources(&f.store, to, f.app, "notes/m.md", OwnerFilter::Any).await,
            vec![a]
        );
    }

    // merge-project: the source project's rows are re-keyed to the target,
    // and survive the source project's deletion.
    {
        let f = fixture().await;
        let target = project(&f.store, f.ws, "target").await;
        let sid = session(&f.store, f.ws, f.app, None).await;
        let a = observe(&f.store, f.ws, f.app, sid, ObservationKind::UserPrompt, 10).await;
        f.store.writer.end_session(sid, None).await.unwrap();
        let path = PagePath::new("notes/merged.md").unwrap();
        let id = f
            .store
            .writer
            .upsert_page(page(f.ws, f.app, path.as_str(), "m", listed(sid, &[a])))
            .await
            .unwrap();
        f.store
            .writer
            .merge_project(
                f.ws,
                f.app,
                target,
                vec![MergeProjectPage {
                    path: path.clone(),
                    source_page_id: id,
                    target_page_id: None,
                    merged_page: None,
                }],
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            raw_rows(&f.store, id),
            vec![(
                f.ws.as_bytes().to_vec(),
                target.as_bytes().to_vec(),
                a.as_bytes().to_vec()
            )]
        );
        assert_eq!(
            sources(&f.store, f.ws, target, path.as_str(), OwnerFilter::Any).await,
            vec![a]
        );
    }

    // move-session: the session page moves with its session's observations.
    {
        let f = fixture().await;
        let target = project(&f.store, f.ws, "target").await;
        let sid = session(&f.store, f.ws, f.app, None).await;
        let a = observe(&f.store, f.ws, f.app, sid, ObservationKind::UserPrompt, 10).await;
        f.store.writer.end_session(sid, None).await.unwrap();
        let path = format!("sessions/{sid}.md");
        let id = f
            .store
            .writer
            .upsert_page(page(f.ws, f.app, &path, "s", listed(sid, &[a])))
            .await
            .unwrap();
        f.store
            .writer
            .move_session(sid, f.ws, target, PagesMode::Move, None, true)
            .await
            .unwrap();
        assert_eq!(
            raw_rows(&f.store, id),
            vec![(
                f.ws.as_bytes().to_vec(),
                target.as_bytes().to_vec(),
                a.as_bytes().to_vec()
            )]
        );
        assert_eq!(
            sorted(sources(&f.store, f.ws, target, &path, OwnerFilter::Any).await),
            vec![a]
        );
    }
}

/// A deleted page version takes its rows along (the `pages` foreign key).
#[tokio::test]
async fn sources_cascade_with_a_purged_session_page() {
    let f = fixture().await;
    let sid = session(&f.store, f.ws, f.app, None).await;
    let a = observe(&f.store, f.ws, f.app, sid, ObservationKind::UserPrompt, 10).await;
    f.store.writer.end_session(sid, None).await.unwrap();
    // The purge takes the versions whose frontmatter names the session.
    let mut session_page = page(
        f.ws,
        f.app,
        &format!("sessions/{sid}.md"),
        "s",
        listed(sid, &[a]),
    );
    session_page.frontmatter_json = serde_json::json!({ "session_id": sid.to_string() });
    f.store.writer.upsert_page(session_page).await.unwrap();
    assert_eq!(total_rows(&f.store), 1);
    f.store
        .writer
        .purge_session(
            f.ws,
            f.app,
            sid,
            None,
            ai_memory_store::Compaction::Skip,
            ai_memory_store::PurgeMode::Commit,
        )
        .await
        .unwrap();
    assert_eq!(total_rows(&f.store), 0);
}
