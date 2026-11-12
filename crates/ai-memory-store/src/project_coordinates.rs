//! Private indexed matching for static workspace/project coordinates.

use ai_memory_core::repository_identity::{
    IdentitySource, RepositoryIdentity, legacy_basename_name, path_style_name,
};
use ai_memory_core::{ProjectId, WorkspaceId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    AmbiguousMatch, AmbiguousProjectHolder, AmbiguousProjectHolders, StoreError, StoreResult,
};

pub(crate) const MAX_COORDINATE_ROWS: usize = 4;

const MATCH_SQL: &str = "WITH \
     exact AS (SELECT id FROM projects INDEXED BY sqlite_autoindex_projects_2 \
       WHERE workspace_id = ?1 AND name = ?2), \
     canonical AS (SELECT id FROM projects INDEXED BY idx_projects_canonical_name \
       WHERE workspace_id = ?1 AND canonical_name = ?2 AND canonical_name <> '' \
       ORDER BY rowid LIMIT ?5), \
     legacy AS (SELECT id FROM projects INDEXED BY idx_projects_legacy_name \
       WHERE workspace_id = ?1 AND legacy_name = ?2 AND legacy_name <> '' \
       ORDER BY rowid LIMIT ?5), \
     identity_match AS (SELECT id FROM projects INDEXED BY idx_projects_identity \
       WHERE workspace_id = ?1 AND identity = ?3 AND identity <> '' AND ?3 <> ''), \
     target AS (SELECT id FROM projects INDEXED BY sqlite_autoindex_projects_2 \
       WHERE workspace_id = ?1 AND name = ?4 AND ?4 <> ''), \
     target_compat AS (SELECT id FROM projects INDEXED BY idx_projects_canonical_name \
       WHERE workspace_id = ?1 AND canonical_name = ?4 AND canonical_name <> '' AND ?4 <> '' \
       ORDER BY rowid LIMIT ?5), \
     raw_matches AS (\
       SELECT id, 1 AS exact_match, 0 AS canonical_match, 0 AS legacy_match, \
              0 AS identity_match, 0 AS canonical_target FROM exact \
       UNION ALL SELECT id, 0, 1, 0, 0, 0 FROM canonical \
       UNION ALL SELECT id, 0, 0, 1, 0, 0 FROM legacy \
       UNION ALL SELECT id, 0, 0, 0, 1, 0 FROM identity_match \
       UNION ALL SELECT id, 0, 0, 0, 0, 1 FROM target \
       UNION ALL SELECT id, 0, 0, 0, 0, 1 FROM target_compat), \
     unique_matches AS (\
       SELECT id, MAX(exact_match) AS exact_match, MAX(canonical_match) AS canonical_match, \
              MAX(legacy_match) AS legacy_match, MAX(identity_match) AS identity_match, \
              MAX(canonical_target) AS canonical_target \
       FROM raw_matches GROUP BY id \
       ORDER BY identity_match DESC, exact_match DESC, \
                (exact_match OR canonical_match OR legacy_match) DESC, id LIMIT ?5) \
     SELECT p.id, p.name, NULLIF(p.identity, ''), NULLIF(p.identity_source, ''), \
            NULLIF(p.canonical_name, ''), NULLIF(p.legacy_name, ''), \
            m.exact_match, m.canonical_match, m.legacy_match, m.identity_match, m.canonical_target, \
            p.access_mode = 'restricted' \
     FROM unique_matches m JOIN projects p ON p.id = m.id AND p.workspace_id = ?1 \
     ORDER BY m.identity_match DESC, m.exact_match DESC, \
              (m.exact_match OR m.canonical_match OR m.legacy_match) DESC, m.id";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MatchProvenance {
    pub(crate) exact: bool,
    pub(crate) canonical_compat: bool,
    pub(crate) legacy_compat: bool,
    pub(crate) identity: bool,
    pub(crate) canonical_target: bool,
}

impl MatchProvenance {
    pub(crate) fn requested(self) -> bool {
        self.exact || self.canonical_compat || self.legacy_compat
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectCoordinateMatch {
    pub(crate) id: ProjectId,
    pub(crate) current_name: String,
    pub(crate) identity: Option<String>,
    pub(crate) identity_source: Option<String>,
    pub(crate) canonical_name: Option<String>,
    pub(crate) legacy_name: Option<String>,
    pub(crate) restricted: bool,
    pub(crate) provenance: MatchProvenance,
}

impl ProjectCoordinateMatch {
    /// Describe this candidate for an ambiguity error. A restricted project
    /// stays anonymous: the lookup runs before any caller is authorized.
    fn ambiguous_holder(&self, empty: bool) -> AmbiguousProjectHolder {
        let visible = !self.restricted;
        AmbiguousProjectHolder {
            name: visible.then(|| self.current_name.clone()),
            matched_by: if self.provenance.exact {
                AmbiguousMatch::Name
            } else if self.provenance.canonical_compat {
                AmbiguousMatch::CanonicalKey
            } else {
                AmbiguousMatch::LegacyKey
            },
            id: visible.then_some(self.id),
            repository: self.identity.clone().filter(|_| visible),
            empty: visible && empty,
        }
    }
}

/// Whether the project holds nothing a caller could mean by naming it: no
/// sessions, observations, pages, handoffs or workstreams.
fn project_is_empty(conn: &Connection, id: ProjectId) -> StoreResult<bool> {
    Ok(conn.query_row(
        "SELECT NOT (EXISTS(SELECT 1 FROM sessions WHERE project_id = ?1) \
           OR EXISTS(SELECT 1 FROM observations WHERE project_id = ?1) \
           OR EXISTS(SELECT 1 FROM pages WHERE project_id = ?1) \
           OR EXISTS(SELECT 1 FROM handoffs WHERE project_id = ?1) \
           OR EXISTS(SELECT 1 FROM workstreams WHERE project_id = ?1))",
        [id.as_bytes()],
        |row| row.get(0),
    )?)
}

/// The one ambiguity that has a single sensible answer (#2018): the requested
/// name is the canonical key (the full remote-derived name static clients are
/// told to derive) of exactly one candidate that owns a repository identity,
/// and every other candidate is an empty, identity-less project. Such a
/// leftover row cannot be what a caller means by the repository's own name,
/// and nothing can be read from it or lost by skipping it.
///
/// A legacy (basename) key match stays ambiguous: a marker may declare that
/// folder-style name for a project that is simply not used yet (#1144). Any
/// restricted candidate keeps the name ambiguous too, so an unauthorized
/// caller never learns which project wins.
fn identity_owner_over_empty_twins(
    candidates: &[ProjectCoordinateMatch],
    empty: &[bool],
) -> Option<usize> {
    if candidates.iter().any(|candidate| candidate.restricted) {
        return None;
    }
    let mut owners = candidates.iter().enumerate().filter(|(_, candidate)| {
        candidate.identity.is_some() && candidate.provenance.canonical_compat
    });
    let (owner, _) = owners.next()?;
    if owners.next().is_some() {
        return None;
    }
    candidates
        .iter()
        .zip(empty)
        .enumerate()
        .all(|(index, (candidate, empty))| {
            index == owner || (*empty && candidate.identity.is_none())
        })
        .then_some(owner)
}

fn ambiguous(name: &str, holders: AmbiguousProjectHolders) -> StoreError {
    StoreError::ProjectNameAmbiguous {
        name: name.to_owned(),
        holders,
    }
}

pub(crate) fn matches(
    conn: &Connection,
    workspace_id: WorkspaceId,
    requested: &str,
    repository: Option<&RepositoryIdentity>,
) -> StoreResult<Vec<ProjectCoordinateMatch>> {
    matches_bounded(
        conn,
        workspace_id,
        requested,
        repository,
        MAX_COORDINATE_ROWS,
    )
}

/// The same indexed coordinate match with a caller-owned result bound.
pub(crate) fn matches_bounded(
    conn: &Connection,
    workspace_id: WorkspaceId,
    requested: &str,
    repository: Option<&RepositoryIdentity>,
    max_rows: usize,
) -> StoreResult<Vec<ProjectCoordinateMatch>> {
    let identity = repository.map_or("", |repository| repository.identity.as_str());
    let canonical = repository.and_then(path_style_name).unwrap_or_default();
    let limit = i64::try_from(max_rows).unwrap_or(i64::MAX);
    let mut statement = conn.prepare(MATCH_SQL)?;
    let rows = statement
        .query_map(
            params![
                workspace_id.as_bytes(),
                requested,
                identity,
                canonical,
                limit
            ],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, bool>(11)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(
            |(
                raw_id,
                current_name,
                identity,
                identity_source,
                canonical_name,
                legacy_name,
                exact,
                canonical_compat,
                legacy_compat,
                identity_match,
                canonical_target,
                restricted,
            )| {
                Ok(ProjectCoordinateMatch {
                    id: ProjectId::from_slice(&raw_id)?,
                    current_name,
                    identity,
                    identity_source,
                    canonical_name,
                    legacy_name,
                    restricted,
                    provenance: MatchProvenance {
                        exact: exact != 0,
                        canonical_compat: canonical_compat != 0,
                        legacy_compat: legacy_compat != 0,
                        identity: identity_match != 0,
                        canonical_target: canonical_target != 0,
                    },
                })
            },
        )
        .collect()
}

/// Whether this connection's schema carries the derived coordinate keys
/// (`canonical_name`/`legacy_name`, V76).
///
/// Every project-coordinate query indexes those columns, so it cannot run
/// against a store an in-flight migration has not reached yet — which is what
/// the migration fixtures do when they build a project row on the old schema.
/// The lookup falls back to a plain insert there, exactly as it always did.
pub(crate) fn supports_coordinates(conn: &Connection) -> StoreResult<bool> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM pragma_table_info('projects') WHERE name = 'legacy_name'")?
        .exists([])?)
}

pub(crate) fn resolve(
    conn: &Connection,
    workspace_id: WorkspaceId,
    requested: &str,
) -> StoreResult<Option<ProjectCoordinateMatch>> {
    let mut matches = matches(conn, workspace_id, requested, None)?
        .into_iter()
        .filter(|candidate| candidate.provenance.requested())
        .collect::<Vec<_>>();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => {
            let empty = matches
                .iter()
                .map(|candidate| project_is_empty(conn, candidate.id))
                .collect::<StoreResult<Vec<_>>>()?;
            if let Some(owner) = identity_owner_over_empty_twins(&matches, &empty) {
                return Ok(Some(matches.swap_remove(owner)));
            }
            Err(ambiguous(
                requested,
                AmbiguousProjectHolders(
                    matches
                        .iter()
                        .zip(&empty)
                        .map(|(candidate, empty)| candidate.ambiguous_holder(*empty))
                        .collect(),
                ),
            ))
        }
    }
}

fn row_to_match(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectCoordinateMatch> {
    Ok(ProjectCoordinateMatch {
        id: ProjectId::from_slice(&row.get::<_, Vec<u8>>(0)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Blob,
                Box::new(error),
            )
        })?,
        current_name: row.get(1)?,
        identity: row.get(2)?,
        identity_source: row.get(3)?,
        canonical_name: row.get(4)?,
        legacy_name: row.get(5)?,
        restricted: row.get(6)?,
        provenance: MatchProvenance {
            exact: true,
            ..MatchProvenance::default()
        },
    })
}

pub(crate) fn resolve_aliases(
    conn: &Connection,
    workspace_id: WorkspaceId,
    canonical: &str,
    aliases: &ai_memory_core::repository_identity::MarkerAliases,
    repository: &RepositoryIdentity,
) -> StoreResult<Option<ProjectCoordinateMatch>> {
    if repository.source != IdentitySource::GitRemote || aliases.is_empty() {
        return Err(ambiguous(canonical, AmbiguousProjectHolders::default()));
    }
    let canonical_match = conn
        .query_row(
            "SELECT id, name, NULLIF(identity, ''), NULLIF(identity_source, ''), \
             NULLIF(canonical_name, ''), NULLIF(legacy_name, ''), \
             access_mode = 'restricted' FROM projects \
             INDEXED BY sqlite_autoindex_projects_2 WHERE workspace_id = ?1 AND name = ?2",
            params![workspace_id.as_bytes(), canonical],
            row_to_match,
        )
        .optional()?;
    if let Some(canonical_match) = canonical_match {
        if canonical_match.identity.as_deref() == Some(repository.identity.as_str())
            && canonical_match.identity_source.as_deref()
                == Some(IdentitySource::GitRemote.as_str())
        {
            return Ok(Some(canonical_match));
        }
        return Err(ambiguous(canonical, AmbiguousProjectHolders::default()));
    }
    let mut candidates = Vec::new();
    for alias in aliases.as_slice() {
        let local = conn
            .query_row(
                "SELECT id, name, NULLIF(identity, ''), NULLIF(identity_source, ''), \
                 NULLIF(canonical_name, ''), NULLIF(legacy_name, ''), \
                 access_mode = 'restricted' FROM projects \
                 INDEXED BY sqlite_autoindex_projects_2 WHERE workspace_id = ?1 AND name = ?2",
                params![workspace_id.as_bytes(), alias],
                row_to_match,
            )
            .optional()?;
        if let Some(candidate) = local {
            candidates.push(candidate);
        }
    }
    candidates.sort_by_key(|candidate| candidate.id.to_string());
    candidates.dedup_by_key(|candidate| candidate.id);
    if candidates.len() > 1 {
        return Err(ambiguous(canonical, AmbiguousProjectHolders::default()));
    }
    let Some(candidate) = candidates.pop() else {
        return Ok(None);
    };
    if candidate.identity.as_deref() != Some(repository.identity.as_str())
        || candidate.identity_source.as_deref() != Some(IdentitySource::GitRemote.as_str())
    {
        return Err(ambiguous(canonical, AmbiguousProjectHolders::default()));
    }
    Ok(Some(candidate))
}

/// An empty, identity-less project whose name is another project's canonical
/// key: a leftover from before new rows were refused under another
/// project's key (#2018). Name resolution already skips it; startup reports
/// it so the operator can rename it out of the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShadowingTwin {
    /// Workspace name.
    pub(crate) workspace: String,
    /// The empty project's id.
    pub(crate) id: ProjectId,
    /// Its name, which is also the owner's canonical key.
    pub(crate) name: String,
    /// The repository owner's current name.
    pub(crate) owner: String,
}

/// Report up to `limit` [`ShadowingTwin`]s. Read-only.
pub(crate) fn shadowing_empty_twins(
    conn: &Connection,
    limit: usize,
) -> StoreResult<Vec<ShadowingTwin>> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut statement = conn.prepare(
        "SELECT w.name, t.id, t.name, o.name FROM projects t \
         JOIN projects o ON o.workspace_id = t.workspace_id AND o.id <> t.id \
           AND o.canonical_name = t.name AND o.identity <> '' \
         JOIN workspaces w ON w.id = t.workspace_id \
         WHERE t.identity = '' AND t.access_mode <> 'restricted' AND o.access_mode <> 'restricted' \
           AND NOT EXISTS(SELECT 1 FROM sessions WHERE project_id = t.id) \
           AND NOT EXISTS(SELECT 1 FROM observations WHERE project_id = t.id) \
           AND NOT EXISTS(SELECT 1 FROM pages WHERE project_id = t.id) \
           AND NOT EXISTS(SELECT 1 FROM handoffs WHERE project_id = t.id) \
           AND NOT EXISTS(SELECT 1 FROM workstreams WHERE project_id = t.id) \
         ORDER BY w.name, t.name LIMIT ?1",
    )?;
    let rows = statement
        .query_map([limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(workspace, id, name, owner)| {
            Ok(ShadowingTwin {
                workspace,
                id: ProjectId::from_slice(&id)?,
                name,
                owner,
            })
        })
        .collect()
}

pub(crate) fn promotion_target<'a>(
    candidate: &'a ProjectCoordinateMatch,
    requested: &str,
) -> Option<&'a str> {
    candidate
        .canonical_name
        .as_deref()
        .filter(|canonical| *canonical == requested && *canonical != candidate.current_name)
}

pub(crate) fn backfill(conn: &mut Connection) -> StoreResult<u64> {
    let mut updated = 0_u64;
    loop {
        let tx = conn.transaction()?;
        let rows = {
            let mut statement = tx.prepare(
                "SELECT id, identity FROM projects INDEXED BY idx_projects_coordinate_backfill \
                 WHERE identity_source = 'git_remote' AND identity <> '' \
                   AND (canonical_name = '' OR legacy_name = '') LIMIT 256",
            )?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if rows.is_empty() {
            return Ok(updated);
        }
        for (raw_id, identity) in rows {
            let remote = RepositoryIdentity {
                identity,
                source: IdentitySource::GitRemote,
            };
            let canonical_name = path_style_name(&remote).ok_or_else(|| {
                StoreError::MalformedRecord("remote has no canonical name".into())
            })?;
            let legacy_name = legacy_basename_name(&remote)
                .ok_or_else(|| StoreError::MalformedRecord("remote has no basename".into()))?;
            updated += u64::try_from(tx.execute(
                "UPDATE projects SET canonical_name = ?1, legacy_name = ?2 WHERE id = ?3",
                params![canonical_name, legacy_name, raw_id],
            )?)
            .unwrap_or(0);
        }
        tx.commit()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Startup reports an empty, identity-less row holding another project's
    /// canonical key, and only that: not once it holds data, and not for a
    /// legacy (basename) key, which a marker may declare on purpose (#2018).
    #[test]
    fn shadowing_empty_twins_reports_only_empty_canonical_key_holders() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::migrations::run(&mut conn).unwrap();
        let workspace = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let owner = crate::ops::get_or_create_project(&mut conn, &workspace, "api", None).unwrap();
        conn.execute(
            "UPDATE projects SET identity = 'github.com/acme/api', identity_source = 'git_remote', \
             canonical_name = 'acme-api', legacy_name = 'api-legacy' WHERE id = ?1",
            [owner.as_bytes()],
        )
        .unwrap();
        let twin = ProjectId::new();
        let legacy_twin = ProjectId::new();
        for (id, name) in [(twin, "acme-api"), (legacy_twin, "api-legacy")] {
            conn.execute(
                "INSERT INTO projects (id, workspace_id, name, created_at) VALUES (?1, ?2, ?3, 0)",
                params![id.as_bytes(), workspace.as_bytes(), name],
            )
            .unwrap();
        }
        assert_eq!(
            shadowing_empty_twins(&conn, 10).unwrap(),
            [ShadowingTwin {
                workspace: "default".into(),
                id: twin,
                name: "acme-api".into(),
                owner: "api".into(),
            }]
        );
        crate::ops::begin_session(
            &mut conn,
            &ai_memory_core::NewSession {
                occurred_at: None,
                id: ai_memory_core::SessionId::new(),
                workspace_id: workspace,
                project_id: twin,
                agent_kind: ai_memory_core::AgentKind::Codex,
                cwd: None,
                actor_user: None,
                origin: None,
            },
        )
        .unwrap();
        assert!(shadowing_empty_twins(&conn, 10).unwrap().is_empty());
    }

    #[test]
    fn shared_candidate_query_deduplicates_before_bound_and_uses_only_indexes() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::migrations::run(&mut conn).unwrap();
        let workspace = crate::ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let repository = RepositoryIdentity {
            identity: "github.com/acme/api".into(),
            source: IdentitySource::GitRemote,
        };
        crate::ops::resolve_project_by_identity(
            &mut conn,
            &workspace,
            &repository,
            ai_memory_core::repository_identity::IdentityStyle::Path,
            false,
            "api",
            None,
            None,
            None,
            crate::AccessMode::Open,
        )
        .unwrap();
        for index in 0..20_u32 {
            conn.execute(
                "INSERT INTO projects (id, workspace_id, name, created_at, identity, \
                 identity_source, canonical_name, legacy_name) \
                 VALUES (?1, ?2, ?3, 1, ?4, 'git_remote', 'collision', 'collision')",
                params![
                    ProjectId::new().as_bytes(),
                    workspace.as_bytes(),
                    format!("collision-{index}"),
                    format!("forge-{index}.example/acme/api")
                ],
            )
            .unwrap();
        }
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {MATCH_SQL}"))
            .unwrap()
            .query_map(
                params![
                    workspace.as_bytes(),
                    "api",
                    repository.identity,
                    "acme-api",
                    i64::try_from(MAX_COORDINATE_ROWS).unwrap()
                ],
                |row| row.get(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(MATCH_SQL.contains("GROUP BY id"));
        assert!(MATCH_SQL.contains("LIMIT ?5"));
        let candidates = matches(&conn, workspace, "collision", None).unwrap();
        assert_eq!(candidates.len(), MAX_COORDINATE_ROWS);
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.id)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            MAX_COORDINATE_ROWS
        );
        assert!(matches!(
            resolve(&conn, workspace, "collision"),
            Err(StoreError::ProjectNameAmbiguous { .. })
        ));
        for index in [
            "sqlite_autoindex_projects_2",
            "idx_projects_canonical_name",
            "idx_projects_legacy_name",
            "idx_projects_identity",
        ] {
            assert!(
                plan.iter().any(|line| line.contains(index)),
                "{index}: {plan:?}"
            );
        }
        assert!(
            plan.iter().all(|line| !line.contains("SCAN projects")),
            "{plan:?}"
        );
    }
}
