//! Per-project grants and access modes: storage (#708).
//!
//! The decision lives in [`crate::project_authz`]; this module keeps what it
//! reads — `project_grants` rows, `workspace_grants` rows and
//! `projects.access_mode` — and the
//! root-only management that changes them, each change recorded in
//! `audit_log`.

use ai_memory_core::{ProjectId, UserId, WorkspaceId};
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::StoreResult;
use crate::project_authz::{AccessMode, GrantLevel};

fn ts(micros: i64) -> Timestamp {
    Timestamp::from_microsecond(micros).unwrap_or(Timestamp::UNIX_EPOCH)
}

/// One user's access to one project's memory — a row of `project_grants`.
///
/// What is granted is access to the project's *memory*, not to the code it
/// may describe: whoever hosts the repository decides that, and ai-memory
/// neither checks nor implies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectGrant {
    /// The workspace the project lives in.
    pub workspace_id: WorkspaceId,
    /// Which project's memory.
    pub project_id: ProjectId,
    /// Who may reach it. A person, not a device: revoking one laptop's key
    /// must not cost them access from another.
    pub user_id: UserId,
    /// What this grant permits.
    pub level: GrantLevel,
    /// Who granted it; `None` for the root bearer token, which authenticates
    /// from configuration and has no `users` row to name.
    pub granted_by: Option<UserId>,
    /// When it was granted, or last changed level.
    pub granted_at: Timestamp,
}

/// The access mode of a repository, or `None` when there is no such project.
///
/// A stored value this build does not recognise reads as `open`, as it does at
/// the choke point ([`AccessMode::from_db`]).
///
/// # Errors
/// Propagates SQL errors.
pub fn access_mode_of(
    conn: &Connection,
    repository_id: ProjectId,
) -> StoreResult<Option<AccessMode>> {
    let stored: Option<Option<String>> = conn
        .query_row(
            "SELECT access_mode FROM projects WHERE id = ?1",
            params![repository_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(stored.map(|mode| AccessMode::from_db(mode.as_deref())))
}

/// Set a repository's access mode, returning the mode it had. `None` when the
/// repository does not exist. Actual changes and their audit records commit
/// together; repeats write nothing.
///
/// # Errors
/// Propagates SQL errors.
pub fn set_access_mode(
    conn: &Connection,
    repository_id: ProjectId,
    mode: AccessMode,
    changed_by: Option<UserId>,
    now: i64,
) -> StoreResult<Option<AccessMode>> {
    let tx = conn.unchecked_transaction()?;
    let Some(previous) = access_mode_of(&tx, repository_id)? else {
        return Ok(None);
    };
    if previous == mode {
        return Ok(Some(previous));
    }
    let workspace: Vec<u8> = tx.query_row(
        "SELECT workspace_id FROM projects WHERE id = ?1",
        params![repository_id.as_bytes()],
        |row| row.get(0),
    )?;
    let workspace_id = WorkspaceId::from_slice(&workspace)?;
    tx.execute(
        "UPDATE projects SET access_mode = ?1 WHERE id = ?2",
        params![mode.as_str(), repository_id.as_bytes()],
    )?;
    audit_grant(
        &tx,
        "set_project_access",
        workspace_id,
        Some(repository_id),
        changed_by,
        &serde_json::json!({"previous": previous.as_str(), "mode": mode.as_str()}),
        now,
    )?;
    tx.commit()?;
    Ok(Some(previous))
}

/// Users who have written to `repository_id` and would not be admitted if it
/// were restricted: enabled, not root, not its creator, and holding no grant
/// on it or on its workspace.
///
/// What an operator needs to see when restricting a project — the people who
/// were working in it and are about to be refused — so they can grant the ones
/// who should stay. "Written to" is page authorship, the attribution upstream
/// already records.
///
/// # Errors
/// Propagates SQL errors.
pub fn authors_without_grant(
    conn: &Connection,
    repository_id: ProjectId,
) -> StoreResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT u.username FROM pages p \
           JOIN users u ON u.id = p.author_id \
          WHERE p.project_id = ?1 \
            AND u.role <> 'root' AND u.disabled_at IS NULL \
            AND NOT EXISTS (SELECT 1 FROM projects pr \
                             WHERE pr.id = ?1 AND pr.created_by = u.id) \
            AND NOT EXISTS (SELECT 1 FROM project_grants pg \
                             WHERE pg.user_id = u.id AND pg.project_id = ?1) \
            AND NOT EXISTS (SELECT 1 FROM workspace_grants wg \
                             JOIN projects wp ON wp.workspace_id = wg.workspace_id \
                            WHERE wg.user_id = u.id AND wp.id = ?1 \
                              AND wp.name NOT LIKE '\\_profile.%' ESCAPE '\\') \
          ORDER BY u.username",
    )?;
    let names = stmt
        .query_map(params![repository_id.as_bytes()], |row| row.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(names)
}

/// The grant this user holds on this project, if any — at most one, the
/// table's primary key being `(workspace, project, user)`.
///
/// A row that cannot be parsed is **skipped and logged**, never repaired into
/// something plausible. Fabricating a level for a malformed row in an
/// authorization table risks inventing access that nobody granted; dropping it
/// can only ever deny, which is the safe direction to fail.
///
/// # Errors
/// Propagates any SQL error.
pub fn grants_for(
    conn: &Connection,
    user_id: UserId,
    project_id: ProjectId,
) -> StoreResult<Vec<ProjectGrant>> {
    let mut stmt = conn.prepare(
        "SELECT workspace_id, level, granted_by, granted_at \
           FROM project_grants WHERE user_id = ?1 AND project_id = ?2",
    )?;
    let rows = stmt.query_map(params![user_id.as_bytes(), project_id.as_bytes()], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<Vec<u8>>>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    let mut grants = Vec::new();
    for row in rows {
        let (workspace, level, by, at) = row?;
        // An ABSENT granter is the root-token case and parses fine. One that
        // is present but unreadable does not: a value we cannot read is never
        // guessed at, and skipping can only ever deny.
        let granter = match &by {
            None => Some(None),
            Some(raw) => UserId::from_slice(raw).ok().map(Some),
        };
        let parsed = WorkspaceId::from_slice(&workspace)
            .ok()
            .zip(granter)
            .zip(GrantLevel::from_db(&level));
        let Some(((workspace_id, granted_by), level)) = parsed else {
            tracing::error!(
                %level,
                "skipping a malformed project_grants row; access will be denied \
                 as though it were absent",
            );
            continue;
        };
        grants.push(ProjectGrant {
            workspace_id,
            project_id,
            user_id,
            level,
            granted_by,
            granted_at: ts(at),
        });
    }
    Ok(grants)
}

/// What a call to [`grant`] actually did.
///
/// Returned rather than inferred so an operator's tooling can say "already had
/// that" instead of reporting a change it did not make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantOutcome {
    /// There was no grant; one was created.
    Granted,
    /// A grant existed at a different level and was changed.
    LevelChanged {
        /// The level held before this call.
        from: GrantLevel,
    },
    /// A grant at exactly this level already existed. Nothing written.
    Unchanged,
}

/// Record a grant change in `audit_log`, the durable answer to "who could
/// reach this, since when, and who decided". The grant table itself only
/// holds what is in force now.
fn audit_grant(
    conn: &Connection,
    op: &str,
    workspace_id: WorkspaceId,
    project_id: Option<ProjectId>,
    by: Option<UserId>,
    detail: &serde_json::Value,
    now: i64,
) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO audit_log (at, op, workspace_id, project_id, page_id, author_id, detail) \
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
        params![
            now,
            op,
            workspace_id.as_bytes(),
            project_id.map(|project| project.as_bytes().to_vec()),
            by.map(|by| by.as_bytes().to_vec()),
            detail.to_string(),
        ],
    )?;
    Ok(())
}

/// Grant `user_id` `level` on `project_id`, or change the level they hold.
///
/// `granted_by` is `None` for the root bearer token. Every change is recorded in `audit_log` (`grant_access`, with the previous
/// level when there was one).
///
/// # Errors
/// [`crate::StoreError::NotFound`] when the project does not exist; otherwise
/// propagates any SQL error.
pub fn grant(
    conn: &Connection,
    user_id: UserId,
    project_id: ProjectId,
    level: GrantLevel,
    granted_by: Option<UserId>,
    now: i64,
) -> StoreResult<GrantOutcome> {
    let held = grants_for(conn, user_id, project_id)?.into_iter().next();
    let outcome = match &held {
        Some(held) if held.level == level => return Ok(GrantOutcome::Unchanged),
        Some(held) => GrantOutcome::LevelChanged { from: held.level },
        None => GrantOutcome::Granted,
    };
    let workspace_id: Option<Vec<u8>> = conn
        .query_row(
            "SELECT workspace_id FROM projects WHERE id = ?1",
            params![project_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    let workspace_id = workspace_id
        .and_then(|bytes| WorkspaceId::from_slice(&bytes).ok())
        .ok_or_else(|| crate::StoreError::NotFound(format!("project {project_id}")))?;
    conn.execute(
        "INSERT INTO project_grants \
         (workspace_id, project_id, user_id, level, granted_by, granted_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT (workspace_id, project_id, user_id) DO UPDATE SET \
           level = excluded.level, granted_by = excluded.granted_by, \
           granted_at = excluded.granted_at",
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            user_id.as_bytes(),
            level.as_str(),
            granted_by.map(|by| by.as_bytes().to_vec()),
            now,
        ],
    )?;
    let previous = held.map(|held| held.level.as_str());
    audit_grant(
        conn,
        "grant_access",
        workspace_id,
        Some(project_id),
        granted_by,
        &serde_json::json!({
            "user_id": user_id.to_string(),
            "level": level.as_str(),
            "previous": previous,
        }),
        now,
    )?;
    Ok(outcome)
}

/// Take away whatever `user_id` holds on `project_id`.
///
/// Returns whether anything was actually revoked, so revoking twice is
/// harmless and still reports honestly. The revocation is recorded in
/// `audit_log` (`revoke_access`); `revoked_by` is `None` for the root bearer
/// token, and must never be able to prevent a revocation — taking access away
/// is what most needs to work when something has gone wrong.
///
/// # Errors
/// Propagates any SQL error.
pub fn revoke(
    conn: &Connection,
    user_id: UserId,
    project_id: ProjectId,
    revoked_by: Option<UserId>,
    now: i64,
) -> StoreResult<bool> {
    let Some(held) = grants_for(conn, user_id, project_id)?.into_iter().next() else {
        return Ok(false);
    };
    conn.execute(
        "DELETE FROM project_grants WHERE user_id = ?1 AND project_id = ?2",
        params![user_id.as_bytes(), project_id.as_bytes()],
    )?;
    audit_grant(
        conn,
        "revoke_access",
        held.workspace_id,
        Some(project_id),
        revoked_by,
        &serde_json::json!({
            "user_id": user_id.to_string(),
            "level": held.level.as_str(),
        }),
        now,
    )?;
    Ok(true)
}

/// One row of the operator's grant listing, resolved to names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantListing {
    /// Who holds it.
    pub username: String,
    /// The workspace the project lives in.
    pub workspace: String,
    /// The project, by the name an operator would type.
    pub project: String,
    /// What they hold.
    pub level: GrantLevel,
}

/// Which grants a listing covers.
#[derive(Debug, Clone, Copy)]
pub enum GrantFilter {
    /// Every grant on the server.
    All,
    /// What one user reaches.
    User(UserId),
    /// Who reaches one project.
    Project(ProjectId),
}

/// Grants matching `filter`, ordered for human reading.
///
/// Names rather than ids: a listing an operator cannot read without three
/// further queries is not a listing.
///
/// # Errors
/// Propagates any SQL error.
pub fn list_grants(conn: &Connection, filter: GrantFilter) -> StoreResult<Vec<GrantListing>> {
    let (predicate, bound): (&str, Option<Vec<u8>>) = match filter {
        GrantFilter::All => ("1 = 1", None),
        GrantFilter::User(user) => (
            "project_grants.user_id = ?1",
            Some(user.as_bytes().to_vec()),
        ),
        GrantFilter::Project(project) => (
            "project_grants.project_id = ?1",
            Some(project.as_bytes().to_vec()),
        ),
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT users.username, workspaces.name, projects.name, project_grants.level \
           FROM project_grants \
           JOIN users      ON users.id = project_grants.user_id \
           JOIN projects   ON projects.id = project_grants.project_id \
           JOIN workspaces ON workspaces.id = project_grants.workspace_id \
          WHERE {predicate} \
          ORDER BY workspaces.name, projects.name, users.username"
    ))?;
    let map = |row: &rusqlite::Row<'_>| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    };
    let rows = match bound {
        Some(id) => stmt
            .query_map(params![id], map)?
            .collect::<Result<Vec<_>, _>>()?,
        None => stmt.query_map([], map)?.collect::<Result<Vec<_>, _>>()?,
    };
    let mut out = Vec::new();
    for (username, workspace, project, level) in rows {
        let Some(level) = GrantLevel::from_db(&level) else {
            // Same rule as `grants_for`: a row we cannot read is not repaired
            // into something plausible, even for display.
            tracing::error!(%username, %project, "skipping a malformed project_grants row in listing");
            continue;
        };
        out.push(GrantListing {
            username,
            workspace,
            project,
            level,
        });
    }
    Ok(out)
}

/// The level `user_id` holds on the whole of `workspace_id`, if any — a row of
/// `workspace_grants` (V115).
///
/// A stored level this build cannot read is **skipped and logged**, as
/// [`grants_for`] does: dropping it can only ever deny.
///
/// # Errors
/// Propagates any SQL error.
pub fn workspace_grant_for(
    conn: &Connection,
    user_id: UserId,
    workspace_id: WorkspaceId,
) -> StoreResult<Option<GrantLevel>> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT level FROM workspace_grants WHERE user_id = ?1 AND workspace_id = ?2",
            params![user_id.as_bytes(), workspace_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let level = GrantLevel::from_db(&stored);
    if level.is_none() {
        tracing::error!(
            level = %stored,
            "skipping a malformed workspace_grants row; access will be denied \
             as though it were absent",
        );
    }
    Ok(level)
}

/// Grant `user_id` `level` on every project of `workspace_id`, present and
/// future, or change the level they hold on it.
///
/// Project grants the user already holds in the workspace are left alone: the
/// choke point takes the higher of the two, and revoking the workspace grant
/// later must give back exactly what was held before it.
///
/// `granted_by` is `None` for the root bearer token. Every change is recorded
/// in `audit_log` (`grant_workspace_access`, with the previous level when
/// there was one, and no project).
///
/// # Errors
/// [`crate::StoreError::NotFound`] when the workspace does not exist;
/// otherwise propagates any SQL error.
pub fn grant_workspace(
    conn: &Connection,
    user_id: UserId,
    workspace_id: WorkspaceId,
    level: GrantLevel,
    granted_by: Option<UserId>,
    now: i64,
) -> StoreResult<GrantOutcome> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspaces WHERE id = ?1)",
        params![workspace_id.as_bytes()],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(crate::StoreError::NotFound(format!(
            "workspace {workspace_id}"
        )));
    }
    let held = workspace_grant_for(conn, user_id, workspace_id)?;
    let outcome = match held {
        Some(held) if held == level => return Ok(GrantOutcome::Unchanged),
        Some(held) => GrantOutcome::LevelChanged { from: held },
        None => GrantOutcome::Granted,
    };
    conn.execute(
        "INSERT INTO workspace_grants (workspace_id, user_id, level, granted_by, granted_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT (workspace_id, user_id) DO UPDATE SET \
           level = excluded.level, granted_by = excluded.granted_by, \
           granted_at = excluded.granted_at",
        params![
            workspace_id.as_bytes(),
            user_id.as_bytes(),
            level.as_str(),
            granted_by.map(|by| by.as_bytes().to_vec()),
            now,
        ],
    )?;
    audit_grant(
        conn,
        "grant_workspace_access",
        workspace_id,
        None,
        granted_by,
        &serde_json::json!({
            "user_id": user_id.to_string(),
            "level": level.as_str(),
            "previous": held.map(GrantLevel::as_str),
        }),
        now,
    )?;
    Ok(outcome)
}

/// Take away the workspace grant `user_id` holds on `workspace_id`.
///
/// Project grants the user holds inside the workspace stay in force: they
/// were given one by one and are revoked one by one. Returns whether anything
/// was revoked; the revocation is recorded in `audit_log`
/// (`revoke_workspace_access`).
///
/// # Errors
/// Propagates any SQL error.
pub fn revoke_workspace(
    conn: &Connection,
    user_id: UserId,
    workspace_id: WorkspaceId,
    revoked_by: Option<UserId>,
    now: i64,
) -> StoreResult<bool> {
    // Read the raw row, not the parsed level: a malformed row must still be
    // revocable.
    let held: Option<String> = conn
        .query_row(
            "SELECT level FROM workspace_grants WHERE user_id = ?1 AND workspace_id = ?2",
            params![user_id.as_bytes(), workspace_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(held) = held else {
        return Ok(false);
    };
    conn.execute(
        "DELETE FROM workspace_grants WHERE user_id = ?1 AND workspace_id = ?2",
        params![user_id.as_bytes(), workspace_id.as_bytes()],
    )?;
    audit_grant(
        conn,
        "revoke_workspace_access",
        workspace_id,
        None,
        revoked_by,
        &serde_json::json!({
            "user_id": user_id.to_string(),
            "level": held,
        }),
        now,
    )?;
    Ok(true)
}

/// One row of the operator's workspace-grant listing, resolved to names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGrantListing {
    /// Who holds it.
    pub username: String,
    /// The workspace whose every project it covers.
    pub workspace: String,
    /// What they hold.
    pub level: GrantLevel,
}

/// Which workspace grants a listing covers.
#[derive(Debug, Clone, Copy)]
pub enum WorkspaceGrantFilter {
    /// Every workspace grant on the server.
    All,
    /// The workspaces one user holds.
    User(UserId),
    /// Who holds one workspace.
    Workspace(WorkspaceId),
}

/// Workspace grants matching `filter`, ordered for human reading.
///
/// # Errors
/// Propagates any SQL error.
pub fn list_workspace_grants(
    conn: &Connection,
    filter: WorkspaceGrantFilter,
) -> StoreResult<Vec<WorkspaceGrantListing>> {
    let (predicate, bound): (&str, Option<Vec<u8>>) = match filter {
        WorkspaceGrantFilter::All => ("1 = 1", None),
        WorkspaceGrantFilter::User(user) => (
            "workspace_grants.user_id = ?1",
            Some(user.as_bytes().to_vec()),
        ),
        WorkspaceGrantFilter::Workspace(workspace) => (
            "workspace_grants.workspace_id = ?1",
            Some(workspace.as_bytes().to_vec()),
        ),
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT users.username, workspaces.name, workspace_grants.level \
           FROM workspace_grants \
           JOIN users      ON users.id = workspace_grants.user_id \
           JOIN workspaces ON workspaces.id = workspace_grants.workspace_id \
          WHERE {predicate} \
          ORDER BY workspaces.name, users.username"
    ))?;
    let map = |row: &rusqlite::Row<'_>| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    };
    let rows = match bound {
        Some(id) => stmt
            .query_map(params![id], map)?
            .collect::<Result<Vec<_>, _>>()?,
        None => stmt.query_map([], map)?.collect::<Result<Vec<_>, _>>()?,
    };
    let mut out = Vec::new();
    for (username, workspace, level) in rows {
        let Some(level) = GrantLevel::from_db(&level) else {
            tracing::error!(%username, %workspace, "skipping a malformed workspace_grants row in listing");
            continue;
        };
        out.push(WorkspaceGrantListing {
            username,
            workspace,
            level,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use ai_memory_core::{NewUser, UserRole};

    struct Fixture {
        _tmp: tempfile::TempDir,
        store: Store,
        ws: ai_memory_core::WorkspaceId,
        alice: UserId,
        bob: UserId,
        client: ProjectId,
        personal: ProjectId,
    }

    async fn human(store: &Store, name: &str, role: UserRole) -> UserId {
        store
            .writer
            .create_human_user(
                NewUser {
                    username: name.to_owned(),
                    name: None,
                    email: None,
                },
                role,
                None,
                false,
            )
            .await
            .unwrap()
    }

    async fn fixture() -> Fixture {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        // Grants only decide anything in a restricted project; every project
        // these tests create starts restricted.
        store
            .writer
            .set_new_project_mode(AccessMode::Restricted)
            .await
            .unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let client = store
            .writer
            .get_or_create_project(ws, "client-work", None)
            .await
            .unwrap();
        let personal = store
            .writer
            .get_or_create_project(ws, "personal", None)
            .await
            .unwrap();
        let alice = human(&store, "alice", UserRole::User).await;
        let bob = human(&store, "bob", UserRole::User).await;
        Fixture {
            _tmp: tmp,
            store,
            ws,
            alice,
            bob,
            client,
            personal,
        }
    }

    fn all_rows(store: &Store, user: UserId, project: ProjectId) -> Vec<ProjectGrant> {
        let conn = Connection::open(store.db_path()).unwrap();
        grants_for(&conn, user, project).unwrap()
    }

    /// `(author_id, detail)` of every audit row with this `op`, oldest first.
    fn audit_rows(store: &Store, op: &str) -> Vec<(Option<Vec<u8>>, serde_json::Value)> {
        let conn = Connection::open(store.db_path()).unwrap();
        let mut stmt = conn
            .prepare("SELECT author_id, detail FROM audit_log WHERE op = ?1 ORDER BY id")
            .unwrap();
        stmt.query_map(params![op], |row| {
            let detail: String = row.get(1)?;
            Ok((row.get(0)?, serde_json::from_str(&detail).unwrap()))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    /// One row per user and project: a level change updates it, and every
    /// change — not a repeat — lands in the audit log with who made it.
    #[tokio::test]
    async fn granting_reports_what_it_did_and_every_change_is_audited() {
        let f = fixture().await;
        let w = &f.store.writer;

        assert_eq!(
            w.grant_memory(f.alice, f.client, GrantLevel::Read, Some(f.bob))
                .await
                .unwrap(),
            GrantOutcome::Granted
        );
        assert_eq!(
            w.grant_memory(f.alice, f.client, GrantLevel::Read, Some(f.bob))
                .await
                .unwrap(),
            GrantOutcome::Unchanged,
            "the same level again is not a change"
        );
        assert_eq!(
            w.grant_memory(f.alice, f.client, GrantLevel::Write, Some(f.bob))
                .await
                .unwrap(),
            GrantOutcome::LevelChanged {
                from: GrantLevel::Read
            }
        );

        let rows = all_rows(&f.store, f.alice, f.client);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].level, GrantLevel::Write);
        assert_eq!(rows[0].granted_by, Some(f.bob));

        let audit = audit_rows(&f.store, "grant_access");
        assert_eq!(audit.len(), 2, "grant and level change, not the repeat");
        assert_eq!(audit[0].0, Some(f.bob.as_bytes().to_vec()));
        assert_eq!(audit[0].1["level"], "read");
        assert_eq!(audit[1].1["level"], "write");
        assert_eq!(audit[1].1["previous"], "read");
        assert_eq!(audit[1].1["user_id"], f.alice.to_string());
    }

    /// Revoking deletes the grant and records it; revoking again is honest
    /// about there being nothing to take. A revoked user reads exactly like one
    /// never granted.
    #[tokio::test]
    async fn revoking_is_honest_and_audited() {
        let f = fixture().await;
        let w = &f.store.writer;
        w.grant_memory(f.alice, f.client, GrantLevel::Write, Some(f.bob))
            .await
            .unwrap();

        assert!(
            w.revoke_memory(f.alice, f.client, Some(f.bob))
                .await
                .unwrap()
        );
        assert!(
            !w.revoke_memory(f.alice, f.client, Some(f.bob))
                .await
                .unwrap()
        );

        assert!(all_rows(&f.store, f.alice, f.client).is_empty());
        assert!(
            f.store
                .reader
                .authorize_project(
                    f.ws,
                    f.client,
                    crate::ProjectPrincipal::user(f.alice),
                    true,
                    crate::ProjectAccess::Read,
                )
                .await
                .unwrap()
                .is_err(),
            "a revoked user reads like one never granted"
        );
        let audit = audit_rows(&f.store, "revoke_access");
        assert_eq!(audit.len(), 1, "the no-op second revoke writes nothing");
        assert_eq!(audit[0].1["level"], "write");
    }

    #[tokio::test]
    async fn the_root_token_can_grant_and_revoke_without_a_users_row() {
        // The operator on a bearer-token install has no `users` row. They must
        // still be able to take access away — that is the operation that most
        // needs to work when something has gone wrong.
        let f = fixture().await;
        let w = &f.store.writer;
        w.grant_memory(f.alice, f.client, GrantLevel::Write, None)
            .await
            .unwrap();
        assert_eq!(all_rows(&f.store, f.alice, f.client)[0].granted_by, None);
        assert!(w.revoke_memory(f.alice, f.client, None).await.unwrap());
        assert!(all_rows(&f.store, f.alice, f.client).is_empty());
        for op in ["grant_access", "revoke_access"] {
            assert_eq!(
                audit_rows(&f.store, op)[0].0,
                None,
                "{op} by the root token"
            );
        }
    }

    async fn page(store: &Store, repository: ProjectId, path: &str, body: &str) {
        let workspace = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        store
            .writer
            .upsert_page(ai_memory_core::NewPage {
                workspace_id: workspace,
                project_id: repository,
                path: ai_memory_core::PagePath::new(path).unwrap(),
                title: path.to_owned(),
                body: body.to_owned(),
                tier: ai_memory_core::Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
                expires_at: None,
                entities: Vec::new(),
                evidence: Vec::new(),
                trust_class: None,
            })
            .await
            .unwrap();
    }

    fn paths<T>(hits: &[T], path: impl Fn(&T) -> &str) -> Vec<String> {
        let mut out: Vec<String> = hits.iter().map(|h| path(h).to_owned()).collect();
        out.sort();
        out
    }

    /// The search filter is written in SQL as "any grant"; the choke point on
    /// every other path is `authorize_project(.., ProjectAccess::Read)`. They
    /// agree only because every level admits a read. This pins that, so adding
    /// a level that cannot read breaks a test rather than quietly widening
    /// search.
    #[test]
    fn every_grant_level_can_read_which_is_what_the_search_filter_assumes() {
        for level in [GrantLevel::Read, GrantLevel::Write] {
            let ctx = crate::ProjectAuthz {
                distinguishes_operators: true,
                access_mode: AccessMode::Restricted,
                is_root: false,
                is_creator: false,
                grant: Some(level),
                workspace_grant: None,
                reserved_global: false,
                private_profile: false,
                subject_to_project_grants: true,
            };
            assert!(
                crate::authorize_project(&ctx, crate::ProjectAccess::Read).is_ok(),
                "{level:?}"
            );
        }
    }

    #[tokio::test]
    async fn search_finds_only_what_the_viewer_may_read() {
        let f = fixture().await;
        let r = &f.store.reader;
        page(
            &f.store,
            f.client,
            "secrets/rates.md",
            "confidential day rate",
        )
        .await;
        page(&f.store, f.personal, "notes/rates.md", "personal day rate").await;
        let global = crate::create_global_scope(&f.store.writer).await.unwrap();
        page(
            &f.store,
            global.project_id,
            "prefs/rates.md",
            "shared day rate",
        )
        .await;
        f.store
            .writer
            .grant_memory(f.alice, f.client, GrantLevel::Read, None)
            .await
            .unwrap();

        let as_viewer = |viewer| async move {
            (
                paths(
                    &r.search_pages("rate".into(), 10, viewer).await.unwrap(),
                    |h| h.path.as_str(),
                ),
                paths(
                    &r.search_pages_with_meta(
                        "rate".into(),
                        10,
                        None,
                        viewer,
                        crate::PageFilter::default(),
                    )
                    .await
                    .unwrap(),
                    |h| h.path.as_str(),
                ),
                paths(&r.recent_pages_global(10, viewer).await.unwrap(), |h| {
                    h.path.as_str()
                }),
            )
        };

        // Alice holds a grant on the client project: she finds it, plus the
        // shared global scope, and nothing from the project she was never
        // given.
        let alice = vec!["prefs/rates.md".to_owned(), "secrets/rates.md".to_owned()];
        assert_eq!(
            as_viewer(Some(f.alice)).await,
            (alice.clone(), alice.clone(), alice)
        );

        // Bob holds nothing: only the global scope, which is shared by design.
        let bob = vec!["prefs/rates.md".to_owned()];
        assert_eq!(
            as_viewer(Some(f.bob)).await,
            (bob.clone(), bob.clone(), bob)
        );

        // No viewer — an install with no database users, or root — sees everything, as before.
        let all = vec![
            "notes/rates.md".to_owned(),
            "prefs/rates.md".to_owned(),
            "secrets/rates.md".to_owned(),
        ];
        assert_eq!(as_viewer(None).await, (all.clone(), all.clone(), all));

        // A revoked grant hides the repository again.
        f.store
            .writer
            .revoke_memory(f.alice, f.client, None)
            .await
            .unwrap();
        let revoked = vec!["prefs/rates.md".to_owned()];
        assert_eq!(
            as_viewer(Some(f.alice)).await,
            (revoked.clone(), revoked.clone(), revoked)
        );
    }

    /// A path prefix narrows only what the viewer may already read. Bob names
    /// the prefix of a page in a repository he holds no grant on and gets
    /// nothing; alice, who holds one, gets the page, and bob's own prefix
    /// still works once he is granted his repository.
    #[tokio::test]
    async fn a_filtered_recent_listing_shows_only_what_the_viewer_may_read() {
        let f = fixture().await;
        let r = &f.store.reader;
        page(
            &f.store,
            f.client,
            "secrets/rates.md",
            "confidential day rate",
        )
        .await;
        page(&f.store, f.personal, "notes/rates.md", "personal day rate").await;
        f.store
            .writer
            .grant_memory(f.alice, f.client, GrantLevel::Read, None)
            .await
            .unwrap();
        f.store
            .writer
            .grant_memory(f.bob, f.personal, GrantLevel::Read, None)
            .await
            .unwrap();

        let listed = |viewer, path_prefix: &'static str| async move {
            let filter = crate::PageFilter {
                path_prefix: Some(path_prefix),
                ..crate::PageFilter::default()
            };
            paths(
                &r.recent_pages_global_filtered(10, viewer, filter)
                    .await
                    .unwrap(),
                |h| h.path.as_str(),
            )
        };

        assert!(
            listed(Some(f.bob), "secrets/").await.is_empty(),
            "a prefix must never reach a repository the viewer may not read"
        );
        let secrets = vec!["secrets/rates.md".to_owned()];
        assert_eq!(listed(Some(f.alice), "secrets/").await, secrets);
        assert_eq!(listed(None, "secrets/").await, secrets);
        assert_eq!(
            listed(Some(f.bob), "notes/").await,
            vec!["notes/rates.md".to_owned()]
        );
    }

    #[tokio::test]
    async fn the_limit_counts_only_what_the_viewer_may_see() {
        // Filtering after LIMIT would hand bob a short page: of the first ten
        // matches, most are alice's. Filtering in the query fills his limit
        // from what he can read, and does not tell him how much was hidden.
        //
        // The hidden pages are built to win every ordering these queries use —
        // more of them than the 40 candidates a limit of 10 over-fetches,
        // ranked higher (the needle repeated) and written more recently — so a
        // filter applied after the LIMIT would leave bob with nothing at all.
        let f = fixture().await;
        let r = &f.store.reader;
        for n in 0..12 {
            page(
                &f.store,
                f.personal,
                &format!("visible/{n:02}.md"),
                "needle visible",
            )
            .await;
        }
        for n in 0..60 {
            page(
                &f.store,
                f.client,
                &format!("hidden/{n:02}.md"),
                "needle needle needle needle hidden",
            )
            .await;
        }
        // Without a viewer the hidden pages crowd out every visible one, which
        // is what makes the assertions below mean something.
        let unfiltered = r.search_pages("needle".into(), 10, None).await.unwrap();
        assert!(
            unfiltered
                .iter()
                .all(|h| h.path.as_str().starts_with("hidden/"))
        );
        let unfiltered = r.recent_pages_global(10, None).await.unwrap();
        assert!(unfiltered.iter().all(|h| h.project_name == "client-work"));

        f.store
            .writer
            .grant_memory(f.bob, f.personal, GrantLevel::Read, None)
            .await
            .unwrap();

        let hits = r
            .search_pages("needle".into(), 10, Some(f.bob))
            .await
            .unwrap();
        assert_eq!(hits.len(), 10);
        assert!(hits.iter().all(|h| h.path.as_str().starts_with("visible/")));

        let hits = r
            .search_pages_with_meta(
                "needle".into(),
                10,
                None,
                Some(f.bob),
                crate::PageFilter::default(),
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 10);
        assert!(hits.iter().all(|h| h.project_name == "personal"));

        let hits = r.recent_pages_global(10, Some(f.bob)).await.unwrap();
        assert_eq!(hits.len(), 10);
        assert!(hits.iter().all(|h| h.project_name == "personal"));
    }

    #[tokio::test]
    async fn the_listing_shows_names_and_only_what_is_in_force() {
        let f = fixture().await;
        let w = &f.store.writer;
        w.grant_memory(f.alice, f.client, GrantLevel::Write, None)
            .await
            .unwrap();
        w.grant_memory(f.bob, f.personal, GrantLevel::Read, None)
            .await
            .unwrap();
        w.revoke_memory(f.bob, f.personal, None).await.unwrap();

        let listing = f.store.reader.list_grants(GrantFilter::All).await.unwrap();
        assert_eq!(
            listing,
            vec![GrantListing {
                username: "alice".into(),
                workspace: "default".into(),
                project: "client-work".into(),
                level: GrantLevel::Write,
            }]
        );

        // Filtered: what one user reaches, and who reaches one project.
        w.grant_memory(f.bob, f.client, GrantLevel::Read, None)
            .await
            .unwrap();
        let alice = f
            .store
            .reader
            .list_grants(GrantFilter::User(f.alice))
            .await
            .unwrap();
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].username, "alice");
        let on_client = f
            .store
            .reader
            .list_grants(GrantFilter::Project(f.client))
            .await
            .unwrap();
        let names: Vec<_> = on_client.iter().map(|g| g.username.as_str()).collect();
        assert_eq!(names, ["alice", "bob"]);
        assert!(
            f.store
                .reader
                .list_grants(GrantFilter::Project(f.personal))
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A grant never outlives its project: purging it takes the grants too, as
    /// the design's CASCADE says, and the audit log still says who had access.
    #[tokio::test]
    async fn purging_a_project_takes_its_grants_and_keeps_the_audit_trail() {
        let f = fixture().await;
        f.store
            .writer
            .grant_memory(f.alice, f.client, GrantLevel::Write, None)
            .await
            .unwrap();
        f.store
            .writer
            .purge_project(
                f.ws,
                f.client,
                "default/client-work",
                None,
                false,
                crate::Compaction::Skip,
                crate::PurgeMode::Commit,
            )
            .await
            .expect("a grant in force does not stand in a purge's way");
        let conn = Connection::open(f.store.db_path()).unwrap();
        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_grants WHERE project_id = ?1",
                params![f.client.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(left, 0);
        assert_eq!(audit_rows(&f.store, "grant_access").len(), 1);
    }

    /// Deleting a workspace cascades through its projects to their grants.
    #[tokio::test]
    async fn deleting_a_workspace_takes_its_grants() {
        let f = fixture().await;
        let w = &f.store.writer;
        let team = w.get_or_create_workspace("team-b").await.unwrap();
        let api = w.get_or_create_project(team, "api", None).await.unwrap();
        w.grant_memory(f.alice, api, GrantLevel::Write, None)
            .await
            .unwrap();
        w.delete_workspace(
            team,
            true,
            crate::Compaction::Skip,
            crate::PurgeMode::Commit,
        )
        .await
        .unwrap();
        assert!(all_rows(&f.store, f.alice, api).is_empty());
    }

    /// The hollow-project sweep is upstream's, unchanged: an empty project is
    /// swept after its age cutoff whether or not someone holds a grant on it,
    /// and the grant goes with it.
    #[tokio::test]
    async fn an_empty_project_is_swept_even_when_someone_holds_a_grant() {
        let f = fixture().await;
        let w = &f.store.writer;
        let probe = w.get_or_create_project(f.ws, "probe", None).await.unwrap();
        w.grant_memory(f.alice, probe, GrantLevel::Write, None)
            .await
            .unwrap();
        let mut conn = Connection::open(f.store.db_path()).unwrap();
        conn.execute(
            "UPDATE projects SET created_at = 0 WHERE id = ?1",
            params![probe.as_bytes()],
        )
        .unwrap();
        let swept = crate::ops::sweep_hollow_projects(&mut conn, 1).unwrap();
        assert!(swept.contains(&"probe".to_owned()), "{swept:?}");
        assert!(all_rows(&f.store, f.alice, probe).is_empty());
    }

    #[tokio::test]
    async fn rename_and_a_true_move_keep_the_grants_with_the_project() {
        // Both keep the project id, so the grants stay attached to the same
        // content — the id, not the name, is the key. A move to another
        // workspace carries the grant's workspace along with it.
        let f = fixture().await;
        let w = &f.store.writer;
        w.grant_memory(f.alice, f.client, GrantLevel::Write, None)
            .await
            .unwrap();

        w.rename_project(f.ws, f.client, "client-renamed", None)
            .await
            .unwrap();
        let elsewhere = w.get_or_create_workspace("elsewhere").await.unwrap();
        w.move_project_workspace(f.client, f.ws, elsewhere)
            .await
            .unwrap();

        let grants = all_rows(&f.store, f.alice, f.client);
        assert_eq!(grants[0].workspace_id, elsewhere);
        assert!(
            f.store
                .reader
                .authorize_project(
                    elsewhere,
                    f.client,
                    crate::ProjectPrincipal::user(f.alice),
                    true,
                    crate::ProjectAccess::Write,
                )
                .await
                .unwrap()
                .is_ok()
        );
        let listing = f.store.reader.list_grants(GrantFilter::All).await.unwrap();
        assert_eq!(listing[0].workspace, "elsewhere");
        assert_eq!(listing[0].project, "client-renamed");

        // And deleting the workspace it left does not take it.
        w.delete_workspace(
            f.ws,
            true,
            crate::Compaction::Skip,
            crate::PurgeMode::Commit,
        )
        .await
        .unwrap();
        assert_eq!(all_rows(&f.store, f.alice, f.client).len(), 1);
    }

    /// Both forms of "may read" are built from one SQL template, but they
    /// reach it by different routes — bound parameters and spliced literals —
    /// and a quoting or numbering slip in either would silently widen or
    /// narrow one set of surfaces. This runs both against the same data.
    #[tokio::test]
    async fn the_spliced_predicate_and_the_bound_filter_admit_the_same_repositories() {
        let f = fixture().await;
        let w = &f.store.writer;
        let global = crate::create_global_scope(w).await.unwrap();
        w.grant_memory(f.alice, f.client, GrantLevel::Read, None)
            .await
            .unwrap();
        w.grant_memory(f.bob, f.personal, GrantLevel::Write, None)
            .await
            .unwrap();
        w.revoke_memory(f.bob, f.personal, None).await.unwrap();
        // carol holds a workspace other than the fixture's: both forms must
        // admit its project and neither of the fixture's.
        let carol = human(&f.store, "carol", UserRole::User).await;
        let team = w.get_or_create_workspace("team-b").await.unwrap();
        let api = w.get_or_create_project(team, "api", None).await.unwrap();
        w.grant_workspace(carol, team, GrantLevel::Read, None)
            .await
            .unwrap();

        let conn = Connection::open(f.store.db_path()).unwrap();
        let ids = |sql: String, binds: Vec<rusqlite::types::Value>| -> Vec<ProjectId> {
            let mut stmt = conn.prepare(&sql).unwrap();
            let mut out: Vec<ProjectId> = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                    row.get::<_, Vec<u8>>(0)
                })
                .unwrap()
                .map(|id| ProjectId::from_slice(&id.unwrap()).unwrap())
                .collect();
            out.sort_by_key(|id| *id.as_bytes());
            out
        };
        let sorted = |mut v: Vec<ProjectId>| {
            v.sort_by_key(|id| *id.as_bytes());
            v
        };
        for (viewer, expected) in [
            (f.alice, sorted(vec![f.client, global.project_id])),
            // bob's only grant was revoked: the shared scope is all that is left.
            (f.bob, vec![global.project_id]),
            (carol, sorted(vec![api, global.project_id])),
        ] {
            let (bound, binds) =
                crate::reader::readable_repository_filter("projects.id", Some(viewer), 1);
            let spliced = crate::reader::readable_repository_predicate("projects.id", Some(viewer));
            let via_binds = ids(format!("SELECT id FROM projects WHERE 1 = 1{bound}"), binds);
            let via_splice = ids(
                format!("SELECT id FROM projects WHERE 1 = 1{spliced}"),
                Vec::new(),
            );
            assert_eq!(via_binds, expected, "bound filter");
            assert_eq!(via_splice, via_binds, "the two forms disagree");
        }
        assert!(
            crate::reader::readable_repository_predicate("projects.id", None).is_empty(),
            "no viewer must leave the query exactly as it was"
        );
    }

    fn unowned_handoff(
        workspace_id: ai_memory_core::WorkspaceId,
        project_id: ProjectId,
        summary: &str,
    ) -> ai_memory_core::NewHandoff {
        ai_memory_core::NewHandoff {
            workspace_id,
            project_id,
            from_session_id: None,
            from_agent: ai_memory_core::AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: summary.into(),
            open_questions: Vec::new(),
            next_steps: Vec::new(),
            files_touched: Vec::new(),
            owner_user: None,
        }
    }

    /// An unowned handoff is visible to everyone who can see its repository —
    /// which is exactly why it must not reach someone who cannot. The query
    /// keeps its `LIMIT 1`, so a newer handoff in a hidden repository must not
    /// shadow an older one the viewer is entitled to either.
    #[tokio::test]
    async fn the_workspace_handoff_comes_only_from_readable_repositories() {
        let f = fixture().await;
        let w = &f.store.writer;
        w.grant_memory(f.alice, f.client, GrantLevel::Read, None)
            .await
            .unwrap();
        w.grant_memory(f.bob, f.personal, GrantLevel::Read, None)
            .await
            .unwrap();
        w.insert_handoff(unowned_handoff(f.ws, f.personal, "bob's older baton"))
            .await
            .unwrap();
        w.insert_handoff(unowned_handoff(f.ws, f.client, "alice's client baton"))
            .await
            .unwrap();
        let latest = |viewer| {
            let reader = f.store.reader.clone();
            let ws = f.ws;
            async move {
                reader
                    .latest_open_handoff_for_workspace(ws, ai_memory_core::OwnerFilter::Any, viewer)
                    .await
                    .unwrap()
                    .map(|h| h.content.summary)
            }
        };
        assert_eq!(
            latest(Some(f.alice)).await.as_deref(),
            Some("alice's client baton")
        );
        assert_eq!(
            latest(Some(f.bob)).await.as_deref(),
            Some("bob's older baton"),
            "the newer, hidden handoff shadowed the one bob may read"
        );
        assert_eq!(
            latest(None).await.as_deref(),
            Some("alice's client baton"),
            "no viewer is unchanged"
        );
    }

    /// Health's duplicate list compares titles through an inner query. If only
    /// the outer query were filtered, a hidden page sharing a title would still
    /// make the visible one show up as a duplicate — confirming, by its
    /// presence, a page the viewer cannot see.
    #[tokio::test]
    async fn a_hidden_page_does_not_make_a_visible_one_a_duplicate() {
        let f = fixture().await;
        f.store
            .writer
            .grant_memory(f.alice, f.client, GrantLevel::Read, None)
            .await
            .unwrap();
        for repo in [f.client, f.personal] {
            f.store
                .writer
                .upsert_page(ai_memory_core::NewPage {
                    workspace_id: f.ws,
                    project_id: repo,
                    path: ai_memory_core::PagePath::new("notes/plan.md").unwrap(),
                    title: "Plan".into(),
                    body: "same title in two repositories".into(),
                    tier: ai_memory_core::Tier::Semantic,
                    frontmatter_json: serde_json::json!({}),
                    pinned: false,
                    links: Vec::new(),
                    author_id: None,
                    expires_at: None,
                    entities: Vec::new(),
                    evidence: Vec::new(),
                    trust_class: None,
                })
                .await
                .unwrap();
        }
        let r = &f.store.reader;
        let (_, dups, _) = r
            .memory_health_for_workspace(f.ws, Some(f.alice))
            .await
            .unwrap();
        assert_eq!(dups, 0, "alice's count includes a page she cannot see");
        let detail = r
            .health_detail_for_workspace(f.ws, 10, Some(f.alice))
            .await
            .unwrap();
        assert!(detail.duplicates.is_empty(), "{:?}", detail.duplicates);

        let (_, dups, _) = r.memory_health_for_workspace(f.ws, None).await.unwrap();
        assert_eq!(dups, 1, "no viewer still sees the pair");
        let detail = r.health_detail_for_workspace(f.ws, 10, None).await.unwrap();
        assert_eq!(detail.duplicates.len(), 2);
    }

    // ---- workspace grants (V115) ----

    /// Whether `user` is admitted to `(workspace, project)` at `need`, through
    /// the reader choke point on a server that distinguishes operators.
    async fn admitted(
        store: &Store,
        workspace: WorkspaceId,
        project: ProjectId,
        user: UserId,
        need: crate::ProjectAccess,
    ) -> bool {
        store
            .reader
            .authorize_project(
                workspace,
                project,
                crate::ProjectPrincipal::user(user),
                true,
                need,
            )
            .await
            .unwrap()
            .is_ok()
    }

    async fn page_in(store: &Store, workspace: WorkspaceId, project: ProjectId, path: &str) {
        store
            .writer
            .upsert_page(ai_memory_core::NewPage {
                workspace_id: workspace,
                project_id: project,
                path: ai_memory_core::PagePath::new(path).unwrap(),
                title: path.to_owned(),
                body: "quarterly day rate".to_owned(),
                tier: ai_memory_core::Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
                expires_at: None,
                entities: Vec::new(),
                evidence: Vec::new(),
                trust_class: None,
            })
            .await
            .unwrap();
    }

    /// The boundary a workspace grant draws: every project of that workspace,
    /// one created later included, and no project of any other workspace —
    /// at the choke point and in search alike. Remove the `workspace_grants`
    /// read from `resolve` or from `readable_repository_sql` and the admitted
    /// rows flip; remove the workspace join and the foreign rows flip.
    #[tokio::test]
    async fn a_workspace_grant_reaches_every_project_of_its_workspace_and_no_other() {
        use crate::ProjectAccess::{Read, Write};
        let f = fixture().await;
        let (w, r) = (&f.store.writer, &f.store.reader);
        let other = w.get_or_create_workspace("other-team").await.unwrap();
        let foreign = w.get_or_create_project(other, "api", None).await.unwrap();
        page_in(&f.store, f.ws, f.client, "client/rates.md").await;
        page_in(&f.store, f.ws, f.personal, "personal/rates.md").await;
        page_in(&f.store, other, foreign, "foreign/rates.md").await;
        let found = |viewer| async move {
            paths(
                &r.search_pages("rate".into(), 10, Some(viewer))
                    .await
                    .unwrap(),
                |h| h.path.as_str(),
            )
        };

        // Before the grant: nothing, anywhere.
        assert!(!admitted(&f.store, f.ws, f.client, f.alice, Read).await);
        assert!(found(f.alice).await.is_empty());

        assert_eq!(
            w.grant_workspace(f.alice, f.ws, GrantLevel::Read, None)
                .await
                .unwrap(),
            GrantOutcome::Granted
        );

        // Every project of the workspace, at the level granted and no higher.
        for project in [f.client, f.personal] {
            assert!(admitted(&f.store, f.ws, project, f.alice, Read).await);
            assert!(
                !admitted(&f.store, f.ws, project, f.alice, Write).await,
                "a read grant on the workspace must not admit a write"
            );
        }
        // A project created after the grant is covered without another grant.
        let later = w.get_or_create_project(f.ws, "later", None).await.unwrap();
        assert!(admitted(&f.store, f.ws, later, f.alice, Read).await);

        // The violation: a project of another workspace stays closed, under
        // its own workspace id and under the granted workspace's id.
        assert!(!admitted(&f.store, other, foreign, f.alice, Read).await);
        assert!(
            !admitted(&f.store, f.ws, foreign, f.alice, Read).await,
            "a granted workspace id paired with a foreign project id must not admit"
        );
        // Search agrees with the choke point.
        assert_eq!(
            found(f.alice).await,
            vec!["client/rates.md".to_owned(), "personal/rates.md".to_owned()]
        );
        // Control: a user without the grant is still refused and finds nothing.
        assert!(!admitted(&f.store, f.ws, f.client, f.bob, Read).await);
        assert!(found(f.bob).await.is_empty());

        // Raising the level admits writes; the foreign workspace stays closed.
        assert_eq!(
            w.grant_workspace(f.alice, f.ws, GrantLevel::Write, None)
                .await
                .unwrap(),
            GrantOutcome::LevelChanged {
                from: GrantLevel::Read
            }
        );
        assert!(admitted(&f.store, f.ws, f.client, f.alice, Write).await);
        assert!(!admitted(&f.store, other, foreign, f.alice, Write).await);

        // Revoking closes all of it again.
        assert!(w.revoke_workspace(f.alice, f.ws, None).await.unwrap());
        assert!(!admitted(&f.store, f.ws, f.client, f.alice, Read).await);
        assert!(!admitted(&f.store, f.ws, later, f.alice, Read).await);
        assert!(found(f.alice).await.is_empty());
        assert!(
            !w.revoke_workspace(f.alice, f.ws, None).await.unwrap(),
            "revoking twice reports that nothing was held"
        );
    }

    /// The two kinds of grant are independent rows and the higher level
    /// decides. Revoking one leaves the other exactly as it was.
    #[tokio::test]
    async fn a_project_grant_and_a_workspace_grant_combine_to_the_higher_level() {
        use crate::ProjectAccess::{Read, Write};
        let f = fixture().await;
        let w = &f.store.writer;
        w.grant_memory(f.alice, f.client, GrantLevel::Write, None)
            .await
            .unwrap();
        w.grant_workspace(f.alice, f.ws, GrantLevel::Read, None)
            .await
            .unwrap();

        assert!(
            admitted(&f.store, f.ws, f.client, f.alice, Write).await,
            "a lower workspace grant must not lower a project grant"
        );
        assert!(admitted(&f.store, f.ws, f.personal, f.alice, Read).await);
        assert!(!admitted(&f.store, f.ws, f.personal, f.alice, Write).await);

        // The workspace grant goes; the project grant given before it stays.
        w.revoke_workspace(f.alice, f.ws, None).await.unwrap();
        assert!(admitted(&f.store, f.ws, f.client, f.alice, Write).await);
        assert!(!admitted(&f.store, f.ws, f.personal, f.alice, Read).await);
        assert_eq!(all_rows(&f.store, f.alice, f.client).len(), 1);

        // And the other way round: revoking the project grant leaves the
        // workspace grant in force.
        w.grant_workspace(f.alice, f.ws, GrantLevel::Read, None)
            .await
            .unwrap();
        w.revoke_memory(f.alice, f.client, None).await.unwrap();
        assert!(admitted(&f.store, f.ws, f.client, f.alice, Read).await);
        assert!(!admitted(&f.store, f.ws, f.client, f.alice, Write).await);
    }

    /// A workspace profile is read by every user of the workspace, so a write
    /// there needs root or a write grant on the profile itself. A write grant
    /// on the workspace opens every ordinary project and still not this one.
    #[tokio::test]
    async fn a_workspace_write_grant_does_not_write_the_shared_profile() {
        use crate::ProjectAccess::{Read, Write};
        let f = fixture().await;
        let w = &f.store.writer;
        let profile = w
            .get_or_create_project(
                f.ws,
                ai_memory_core::profile::WORKSPACE_PROFILE_PROJECT,
                None,
            )
            .await
            .unwrap();
        w.grant_workspace(f.alice, f.ws, GrantLevel::Write, None)
            .await
            .unwrap();

        // In both modes. A profile starts open, where no grant is read at
        // all; restricted is where the workspace grant is resolved and the
        // write gate has to refuse it by itself.
        for mode in [AccessMode::Open, AccessMode::Restricted] {
            let conn = Connection::open(f.store.db_path()).unwrap();
            set_access_mode(
                &conn,
                profile,
                mode,
                None,
                Timestamp::now().as_microsecond(),
            )
            .unwrap();
            assert!(
                !admitted(&f.store, f.ws, profile, f.alice, Write).await,
                "a workspace grant must not write the shared profile ({mode:?})"
            );
            // The workspace grant still reads it.
            assert!(admitted(&f.store, f.ws, profile, f.alice, Read).await);
            // bob holds nothing: a restricted profile refuses his read.
            assert_eq!(
                admitted(&f.store, f.ws, profile, f.bob, Read).await,
                mode == AccessMode::Open
            );
        }
        // Controls: the same grant writes an ordinary project, root writes the
        // profile, and so does a write grant on the profile itself.
        assert!(admitted(&f.store, f.ws, f.client, f.alice, Write).await);
        assert!(
            f.store
                .reader
                .authorize_project(f.ws, profile, crate::ProjectPrincipal::root(), true, Write)
                .await
                .unwrap()
                .is_ok()
        );
        w.grant_memory(f.alice, profile, GrantLevel::Write, None)
            .await
            .unwrap();
        assert!(admitted(&f.store, f.ws, profile, f.alice, Write).await);
    }

    /// Every operator's private cross-project profile lives in `default` as
    /// `_profile.<user_id>`. A workspace grant on `default` must never read or
    /// write another operator's private profile, either at `authorize_project`
    /// or in unscoped search.
    #[tokio::test]
    async fn a_workspace_grant_never_reaches_another_operators_private_profile() {
        use crate::ProjectAccess::{Read, Write};
        let f = fixture().await;
        let (w, r) = (&f.store.writer, &f.store.reader);
        let bob_profile_name = ai_memory_core::profile::user_profile_project(f.bob);
        let (bob_profile, _) = w
            .get_or_create_project_as(f.ws, &bob_profile_name, None, Some(f.bob))
            .await
            .unwrap();
        {
            let conn = Connection::open(f.store.db_path()).unwrap();
            set_access_mode(
                &conn,
                bob_profile,
                AccessMode::Restricted,
                None,
                Timestamp::now().as_microsecond(),
            )
            .unwrap();
        }
        page_in(&f.store, f.ws, bob_profile, "profile/secret.md").await;
        page_in(&f.store, f.ws, f.client, "client/rates.md").await;

        w.grant_workspace(f.alice, f.ws, GrantLevel::Write, None)
            .await
            .unwrap();

        // Violation: alice's workspace write grant on `default` must not reach
        // bob's private profile at the choke point or in search.
        assert!(
            !admitted(&f.store, f.ws, bob_profile, f.alice, Read).await,
            "a workspace grant must not read another operator's private profile"
        );
        assert!(
            !admitted(&f.store, f.ws, bob_profile, f.alice, Write).await,
            "a workspace grant must not write another operator's private profile"
        );
        let alice_hits = paths(
            &r.search_pages("rate".into(), 10, Some(f.alice))
                .await
                .unwrap(),
            |h| h.path.as_str(),
        );
        assert_eq!(
            alice_hits,
            vec!["client/rates.md".to_owned()],
            "alice's search must hide bob's private profile page"
        );

        // Control 1: an ordinary restricted project in `default` is reachable
        // via the workspace grant.
        assert!(admitted(&f.store, f.ws, f.client, f.alice, Read).await);
        assert!(admitted(&f.store, f.ws, f.client, f.alice, Write).await);

        // Control 2: the profile owner can still read and write their own
        // private profile and find its pages in search.
        assert!(admitted(&f.store, f.ws, bob_profile, f.bob, Read).await);
        assert!(admitted(&f.store, f.ws, bob_profile, f.bob, Write).await);
        let bob_hits = paths(
            &r.search_pages("rate".into(), 10, Some(f.bob))
                .await
                .unwrap(),
            |h| h.path.as_str(),
        );
        assert_eq!(bob_hits, vec!["profile/secret.md".to_owned()]);

        // Control 3: root can still read and write the private profile.
        for need in [Read, Write] {
            assert!(
                r.authorize_project(
                    f.ws,
                    bob_profile,
                    crate::ProjectPrincipal::root(),
                    true,
                    need,
                )
                .await
                .unwrap()
                .is_ok()
            );
        }
    }

    /// A project follows the grants of the workspace it is in now: moved out,
    /// it leaves the old workspace's grant behind and comes under the new
    /// one's.
    #[tokio::test]
    async fn a_moved_project_follows_the_grants_of_its_new_workspace() {
        use crate::ProjectAccess::Read;
        let f = fixture().await;
        let w = &f.store.writer;
        let elsewhere = w.get_or_create_workspace("elsewhere").await.unwrap();
        w.grant_workspace(f.alice, f.ws, GrantLevel::Write, None)
            .await
            .unwrap();
        w.grant_workspace(f.bob, elsewhere, GrantLevel::Read, None)
            .await
            .unwrap();
        assert!(admitted(&f.store, f.ws, f.client, f.alice, Read).await);

        w.move_project_workspace(f.client, f.ws, elsewhere)
            .await
            .unwrap();

        assert!(
            !admitted(&f.store, elsewhere, f.client, f.alice, Read).await,
            "the old workspace's grant must not follow the project"
        );
        assert!(!admitted(&f.store, f.ws, f.client, f.alice, Read).await);
        assert!(admitted(&f.store, elsewhere, f.client, f.bob, Read).await);
        // Control: what stayed behind is still alice's.
        assert!(admitted(&f.store, f.ws, f.personal, f.alice, Read).await);
    }

    /// One row per user and workspace; every change — not a repeat — is
    /// audited with who made it and no project; the listing shows names.
    #[tokio::test]
    async fn workspace_grants_are_audited_listed_and_deleted_with_their_workspace() {
        let f = fixture().await;
        let (w, r) = (&f.store.writer, &f.store.reader);
        let team = w.get_or_create_workspace("team-b").await.unwrap();

        assert_eq!(
            w.grant_workspace(f.alice, team, GrantLevel::Read, Some(f.bob))
                .await
                .unwrap(),
            GrantOutcome::Granted
        );
        assert_eq!(
            w.grant_workspace(f.alice, team, GrantLevel::Read, Some(f.bob))
                .await
                .unwrap(),
            GrantOutcome::Unchanged
        );
        w.grant_workspace(f.alice, team, GrantLevel::Write, Some(f.bob))
            .await
            .unwrap();
        w.grant_workspace(f.bob, f.ws, GrantLevel::Read, None)
            .await
            .unwrap();

        let granted = audit_rows(&f.store, "grant_workspace_access");
        assert_eq!(granted.len(), 3, "the repeat is not a change");
        assert_eq!(granted[0].0, Some(f.bob.as_bytes().to_vec()));
        assert_eq!(granted[1].1["previous"], "read");
        assert_eq!(granted[1].1["level"], "write");
        assert_eq!(granted[1].1["user_id"], f.alice.to_string());
        let conn = Connection::open(f.store.db_path()).unwrap();
        let with_project: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_log \
                  WHERE op = 'grant_workspace_access' AND project_id IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(with_project, 0, "a workspace grant names no project");

        let listing = |filter| async move {
            r.list_workspace_grants(filter)
                .await
                .unwrap()
                .into_iter()
                .map(|g| (g.username, g.workspace, g.level))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            listing(WorkspaceGrantFilter::All).await,
            vec![
                ("bob".to_owned(), "default".to_owned(), GrantLevel::Read),
                ("alice".to_owned(), "team-b".to_owned(), GrantLevel::Write),
            ]
        );
        assert_eq!(
            listing(WorkspaceGrantFilter::User(f.alice)).await,
            vec![("alice".to_owned(), "team-b".to_owned(), GrantLevel::Write)]
        );
        assert_eq!(
            listing(WorkspaceGrantFilter::Workspace(f.ws)).await,
            vec![("bob".to_owned(), "default".to_owned(), GrantLevel::Read)]
        );

        // An unknown workspace is refused, never created by a grant.
        let ghost = WorkspaceId::new();
        assert!(matches!(
            w.grant_workspace(f.alice, ghost, GrantLevel::Read, None)
                .await,
            Err(crate::StoreError::NotFound(_))
        ));

        // Deleting the workspace takes its grants; the revoke is still audited
        // for the one that was revoked by hand.
        assert!(
            w.revoke_workspace(f.bob, f.ws, Some(f.alice))
                .await
                .unwrap()
        );
        assert_eq!(audit_rows(&f.store, "revoke_workspace_access").len(), 1);
        w.delete_workspace(
            team,
            true,
            crate::Compaction::Skip,
            crate::PurgeMode::Commit,
        )
        .await
        .unwrap();
        assert!(listing(WorkspaceGrantFilter::All).await.is_empty());
    }

    /// Restricting a project warns about the authors it will refuse. An author
    /// who holds the workspace is not one of them.
    #[tokio::test]
    async fn an_author_with_a_workspace_grant_is_not_listed_as_losing_access() {
        let f = fixture().await;
        let (w, r) = (&f.store.writer, &f.store.reader);
        for (author, path) in [(f.alice, "notes/a.md"), (f.bob, "notes/b.md")] {
            w.upsert_page(ai_memory_core::NewPage {
                workspace_id: f.ws,
                project_id: f.client,
                path: ai_memory_core::PagePath::new(path).unwrap(),
                title: path.to_owned(),
                body: "body".to_owned(),
                tier: ai_memory_core::Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: Some(author),
                expires_at: None,
                entities: Vec::new(),
                evidence: Vec::new(),
                trust_class: None,
            })
            .await
            .unwrap();
        }
        assert_eq!(
            r.authors_without_grant(f.client).await.unwrap(),
            vec!["alice".to_owned(), "bob".to_owned()]
        );
        w.grant_workspace(f.alice, f.ws, GrantLevel::Read, None)
            .await
            .unwrap();
        assert_eq!(
            r.authors_without_grant(f.client).await.unwrap(),
            vec!["bob".to_owned()]
        );
    }
}
