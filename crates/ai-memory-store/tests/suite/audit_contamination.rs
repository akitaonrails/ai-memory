//! Integration tests for the structural cross-project contamination audit.
//!
//! Seeds a DB with both contamination signatures (a session whose cwd resolves
//! to a different project, and an observation whose project disagrees with its
//! session) plus clean controls, then asserts `audit_contamination` flags
//! exactly the contaminated rows.

use ai_memory_core::{ProjectId, WorkspaceId};
use ai_memory_store::Store;
use rusqlite::{Connection, params};

fn id(n: u8) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[15] = n;
    b
}

/// Seed the fixture directly via SQL so we control every (ws, proj, cwd) — the
/// writer would derive some of these and we need the deliberate mismatch.
fn seed(db_path: &std::path::Path) {
    let conn = Connection::open(db_path).unwrap();
    let now = 1_700_000_000_000_i64;
    let (ws, a, b) = (id(1), id(2), id(3));
    let (sess_wrong, sess_clean) = (id(10), id(11));
    let (obs_drift, obs_clean) = (id(20), id(21));

    conn.execute(
        "INSERT INTO workspaces (id, name, created_at) VALUES (?1, ?2, ?3)",
        params![&ws[..], "auditws", now],
    )
    .unwrap();
    for (pid, name, rp) in [(&a, "proj-a", "/w/a"), (&b, "proj-b", "/w/b")] {
        conn.execute(
            "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![&pid[..], &ws[..], name, rp, now],
        )
        .unwrap();
    }
    // CHECK A: session landed in proj-a but its cwd is under proj-b's repo_path.
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
         VALUES (?1, ?2, ?3, 'claude-code', ?4, ?5)",
        params![&sess_wrong[..], &ws[..], &a[..], "/w/b/sub", now],
    )
    .unwrap();
    // Clean: session in proj-b, cwd under proj-b → resolves to its own bucket.
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
         VALUES (?1, ?2, ?3, 'claude-code', ?4, ?5)",
        params![&sess_clean[..], &ws[..], &b[..], "/w/b", now],
    )
    .unwrap();
    // CHECK B: observation tagged proj-b but its session (sess_wrong) is in proj-a.
    conn.execute(
        "INSERT INTO observations \
         (id, session_id, workspace_id, project_id, kind, title, body, created_at) \
         VALUES (?1, ?2, ?3, ?4, 'note', 't', 'x', ?5)",
        params![&obs_drift[..], &sess_wrong[..], &ws[..], &b[..], now],
    )
    .unwrap();
    // Clean: observation in proj-a, session in proj-a → no drift.
    conn.execute(
        "INSERT INTO observations \
         (id, session_id, workspace_id, project_id, kind, title, body, created_at) \
         VALUES (?1, ?2, ?3, ?4, 'note', 't', 'x', ?5)",
        params![&obs_clean[..], &sess_wrong[..], &ws[..], &a[..], now],
    )
    .unwrap();
}

#[tokio::test]
async fn audit_flags_wrong_bucket_session_but_not_cross_project_observation() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    seed(store.db_path());

    let report = store.reader.audit_contamination(None, None).await.unwrap();

    assert_eq!(
        report.summary.sessions_misbucketed, 1,
        "exactly the wrong-bucket session is flagged (clean one is not)"
    );

    let a = report
        .findings
        .iter()
        .find(|f| f.check == "session_wrong_bucket")
        .expect("CHECK A finding present");
    assert_eq!(a.confidence, "high");
    assert_eq!(a.entity_kind, "session");
    assert_eq!(a.landed_project, "proj-a");
    assert_eq!(a.expected_project.as_deref(), Some("proj-b"));
    assert_eq!(a.cwd.as_deref(), Some("/w/b/sub"));

    // The observation whose project differs from its session's project (seeded
    // above) is NOT flagged: with per-cwd attribution that is legitimate
    // cross-repo work, not contamination. Only the session-cwd signal fires.
    assert!(
        report
            .findings
            .iter()
            .all(|f| f.entity_kind != "observation"),
        "cross-project observations must not be flagged (per-cwd is legitimate)"
    );
    assert_eq!(report.findings.len(), 1, "only CHECK A fires");
}

#[tokio::test]
async fn audit_clean_db_returns_no_findings() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    // A single clean project + session in its own bucket.
    let conn = Connection::open(store.db_path()).unwrap();
    let now = 1_700_000_000_000_i64;
    let (ws, p, s) = (id(1), id(2), id(3));
    conn.execute(
        "INSERT INTO workspaces (id, name, created_at) VALUES (?1, 'w', ?2)",
        params![&ws[..], now],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
         VALUES (?1, ?2, 'p', '/w/p', ?3)",
        params![&p[..], &ws[..], now],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
         VALUES (?1, ?2, ?3, 'claude-code', '/w/p', ?4)",
        params![&s[..], &ws[..], &p[..], now],
    )
    .unwrap();
    drop(conn);

    let report = store.reader.audit_contamination(None, None).await.unwrap();
    assert_eq!(report.summary.sessions_misbucketed, 0);
    assert!(report.findings.is_empty());
}

#[tokio::test]
async fn audit_ignores_home_repo_path_when_home_is_provided() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let conn = Connection::open(store.db_path()).unwrap();
    let now = 1_700_000_000_000_i64;
    let (ws, home_proj, app_proj, session) = (id(1), id(2), id(3), id(4));

    conn.execute(
        "INSERT INTO workspaces (id, name, created_at) VALUES (?1, 'w', ?2)",
        params![&ws[..], now],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
         VALUES (?1, ?2, 'home', '/home/tester', ?3)",
        params![&home_proj[..], &ws[..], now],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
         VALUES (?1, ?2, 'app', NULL, ?3)",
        params![&app_proj[..], &ws[..], now],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
         VALUES (?1, ?2, ?3, 'claude-code', '/home/tester/projects/app', ?4)",
        params![&session[..], &ws[..], &app_proj[..], now],
    )
    .unwrap();
    drop(conn);

    let without_home = store.reader.audit_contamination(None, None).await.unwrap();
    assert_eq!(
        without_home.summary.sessions_misbucketed, 1,
        "without the home guard, the home repo_path looks like the expected project"
    );

    let with_home = store
        .reader
        .audit_contamination(None, Some("/home/tester"))
        .await
        .unwrap();
    assert_eq!(with_home.summary.sessions_misbucketed, 0);
    assert!(with_home.findings.is_empty());
}

#[tokio::test]
async fn audit_prefix_paths_treat_percent_and_underscore_literally() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let conn = Connection::open(store.db_path()).unwrap();
    let now = 1_700_000_000_000_i64;
    let (ws, underscore, percent, sibling, wrong_session, clean_session, sibling_session) =
        (id(1), id(2), id(3), id(4), id(5), id(6), id(7));

    conn.execute(
        "INSERT INTO workspaces (id, name, created_at) VALUES (?1, 'w', ?2)",
        params![&ws[..], now],
    )
    .unwrap();
    for (pid, name, repo_path) in [
        (&underscore, "under_score", "/w/a_b"),
        (&percent, "per_cent", "/w/a%b"),
        (&sibling, "sibling", "/w/axb"),
    ] {
        conn.execute(
            "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![&pid[..], &ws[..], name, repo_path, now],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
         VALUES (?1, ?2, ?3, 'claude-code', ?4, ?5)",
        params![
            &wrong_session[..],
            &ws[..],
            &underscore[..],
            "/w/a%b/sub",
            now
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
         VALUES (?1, ?2, ?3, 'claude-code', ?4, ?5)",
        params![
            &clean_session[..],
            &ws[..],
            &underscore[..],
            "/w/a_b/sub",
            now
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
         VALUES (?1, ?2, ?3, 'claude-code', ?4, ?5)",
        params![
            &sibling_session[..],
            &ws[..],
            &sibling[..],
            "/w/axb/sub",
            now
        ],
    )
    .unwrap();
    drop(conn);

    let report = store.reader.audit_contamination(None, None).await.unwrap();

    assert_eq!(report.summary.sessions_misbucketed, 1);
    let finding = report
        .findings
        .iter()
        .find(|f| f.check == "session_wrong_bucket")
        .expect("percent repo_path mismatch should be visible to audit");
    assert_eq!(finding.landed_project, "under_score");
    assert_eq!(finding.expected_project.as_deref(), Some("per_cent"));
    assert_eq!(finding.cwd.as_deref(), Some("/w/a%b/sub"));
}

#[tokio::test]
async fn audit_scope_restricts_to_landed_bucket() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    seed(store.db_path());
    let (ws, a) = (id(1), id(2));

    // Scoped to proj-a: only the wrong-bucket session (landed in A) is in scope.
    let scope = Some((
        WorkspaceId::from_slice(&ws).unwrap(),
        ProjectId::from_slice(&a).unwrap(),
    ));
    let report = store.reader.audit_contamination(scope, None).await.unwrap();
    assert_eq!(report.summary.sessions_misbucketed, 1);
    assert!(
        report.findings.iter().all(|f| f.landed_project == "proj-a"),
        "a scoped audit returns only findings landed in the scoped project"
    );
}
