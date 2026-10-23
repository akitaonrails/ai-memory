//! Store side of the cross-project profile
//! (`docs/design-cross-project-profile.md`): the per-project `[profile]`
//! flags (V74) and the bounded reads the SessionStart digest needs.

use std::collections::{BTreeSet, HashMap};

use ai_memory_core::profile::{
    PROFILE_PATH_PREFIX, ProfileCandidateSource, ProfileEntry, ProfileGenerality, stack_tags_in,
};
use ai_memory_core::{AutoImproveProposalId, ProjectId, WorkspaceId};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::error::StoreResult;
use crate::reader::{not_expired, now_us};

/// Most profile entries one digest read loads; the byte budget usually cuts
/// far earlier, and the read stays bounded however large a profile grows.
pub const PROFILE_ENTRIES_LIMIT: usize = 200;

/// Recent observations scanned for stack signals. Bounded so the scan costs
/// the same on a project with a long history as on a new one.
const STACK_SIGNAL_OBSERVATIONS: i64 = 2_000;

/// A project's `[profile]` flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectProfileFlags {
    /// The project may be harvested into the profile.
    pub contribute: bool,
    /// The project receives the profile digest and query union.
    pub consume: bool,
}

impl Default for ProjectProfileFlags {
    fn default() -> Self {
        Self {
            contribute: true,
            consume: true,
        }
    }
}

/// Everything the SessionStart digest needs, read in one bounded pass.
#[derive(Debug, Clone, Default)]
pub struct ProfileDigestInputs {
    /// The profile scope's entries.
    pub entries: Vec<ProfileEntry>,
    /// Stack tags of the project the session is in.
    pub project_tags: BTreeSet<String>,
    /// Whether that project already has pages (false: baseline digest).
    pub project_has_pages: bool,
}

/// A project that opted out of contributing to the profile.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProfileOptOut {
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
}

/// Persist `flags` on `project_id`. The session-start path calls this only
/// when an explicit forwarded flag differs from the stored value; the
/// conditional `WHERE` keeps a racing duplicate write a no-op.
pub(crate) fn set_project_profile_flags(
    conn: &Connection,
    project_id: ProjectId,
    flags: ProjectProfileFlags,
) -> StoreResult<()> {
    conn.execute(
        "UPDATE projects SET profile_contribute = ?2, profile_consume = ?3 \
         WHERE id = ?1 AND (profile_contribute <> ?2 OR profile_consume <> ?3)",
        params![
            project_id.as_bytes(),
            i64::from(flags.contribute),
            i64::from(flags.consume)
        ],
    )?;
    Ok(())
}

/// The stored flags of a project; the defaults for an unknown project.
pub(crate) fn project_profile_flags(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<ProjectProfileFlags> {
    let row: Option<(i64, i64)> = conn
        .query_row(
            "SELECT profile_contribute, profile_consume FROM projects \
             WHERE id = ?1 AND workspace_id = ?2",
            params![project_id.as_bytes(), workspace_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(
        row.map_or_else(ProjectProfileFlags::default, |(contribute, consume)| {
            ProjectProfileFlags {
                contribute: contribute != 0,
                consume: consume != 0,
            }
        }),
    )
}

/// Every project whose marker keeps it out of the profile, by name.
pub(crate) fn contribute_opt_outs(conn: &Connection) -> StoreResult<Vec<ProfileOptOut>> {
    let mut stmt = conn.prepare(
        "SELECT w.name, p.name FROM projects p JOIN workspaces w ON w.id = p.workspace_id \
         WHERE p.profile_contribute = 0 ORDER BY w.name, p.name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ProfileOptOut {
            workspace: row.get(0)?,
            project: row.get(1)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The current, unexpired `profile/` pages of a scope, as digest entries.
/// Pages with nothing to state are skipped.
pub(crate) fn profile_entries(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    limit: usize,
) -> StoreResult<Vec<ProfileEntry>> {
    let limit = i64::try_from(limit.clamp(1, PROFILE_ENTRIES_LIMIT)).unwrap_or(1);
    let sql = format!(
        "SELECT path, title, body, frontmatter_json FROM pages \
         WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1 \
           AND path GLOB ?3{not_expired} \
         ORDER BY path ASC LIMIT ?5",
        not_expired = not_expired("pages", "?4"),
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            format!("{PROFILE_PATH_PREFIX}*"),
            now_us(),
            limit
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        },
    )?;
    let mut entries = Vec::new();
    for row in rows {
        let (path, title, body, frontmatter) = row?;
        let frontmatter = serde_json::from_str(&frontmatter).unwrap_or(serde_json::Value::Null);
        if let Some(entry) = ProfileEntry::from_page(&path, &title, &body, &frontmatter) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Stack tags the project's recent activity shows (file names and source
/// extensions in observation titles), from one bounded read.
pub(crate) fn project_stack_tags(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<BTreeSet<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT title FROM observations WHERE workspace_id = ?1 AND project_id = ?2 \
         ORDER BY created_at DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            STACK_SIGNAL_OBSERVATIONS
        ],
        |row| row.get::<_, String>(0),
    )?;
    let mut tags = BTreeSet::new();
    for title in rows {
        tags.extend(stack_tags_in(&title?).into_iter().map(str::to_owned));
    }
    Ok(tags)
}

/// Whether a project has any current, unexpired page: a project without one is
/// new, and gets the larger baseline digest. A project whose every page has
/// passed its TTL reads as new, the same way the digest's own reads skip
/// expired pages.
pub(crate) fn project_has_pages(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<bool> {
    let sql = format!(
        "SELECT 1 FROM pages WHERE workspace_id = ?1 AND project_id = ?2 \
         AND is_latest = 1{not_expired} LIMIT 1",
        not_expired = not_expired("pages", "?3"),
    );
    Ok(conn
        .query_row(
            &sql,
            params![workspace_id.as_bytes(), project_id.as_bytes(), now_us()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Most user-prompt observations or curated pages one harvest reads from a
/// project in a single pass. A pass resumes from its marks, so a long history
/// is consumed over several passes instead of in one unbounded read.
pub const PROFILE_HARVEST_BATCH: usize = 2_000;

/// Most candidates one convergence reads, newest first. The cap bounds the
/// grouping work; the newest evidence is the one that wins a ruling anyway.
pub const PROFILE_CANDIDATES_LIMIT: usize = 20_000;

/// Curated page prefixes the harvester reads.
const CURATED_PREFIXES: [&str; 4] = ["_rules/", "decisions/", "gotchas/", "procedures/"];

/// How far the harvester has read one project.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProfileHarvestMark {
    /// Newest user-prompt observation already harvested (µs).
    pub observations_until: i64,
    /// Newest curated-page update already harvested (µs).
    pub pages_until: i64,
}

/// A project the harvester may read: it contributes, and it is not a reserved
/// scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileHarvestProject {
    /// Workspace id.
    pub workspace_id: WorkspaceId,
    /// Project id.
    pub project_id: ProjectId,
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// The project is `restricted` (per-project authorization).
    pub restricted: bool,
    /// How far it has been harvested.
    pub mark: ProfileHarvestMark,
}

/// A user prompt to harvest. Its body passed the sanitizer at ingress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilePromptRow {
    /// Session the prompt belongs to.
    pub session_id: ai_memory_core::SessionId,
    /// Capture time (µs).
    pub created_at: i64,
    /// Prompt text.
    pub body: String,
    /// The session's owner (`sessions.actor_user`), `None` when shared.
    pub contributor: Option<String>,
}

/// A curated page to harvest.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfilePageRow {
    /// Wiki path.
    pub path: String,
    /// Title.
    pub title: String,
    /// Body.
    pub body: String,
    /// Frontmatter.
    pub frontmatter: serde_json::Value,
    /// Last update (µs).
    pub updated_at: i64,
    /// `user:<username>` of the page's author, `None` when shared.
    pub contributor: Option<String>,
}

/// A candidate to record.
#[derive(Debug, Clone, PartialEq)]
pub struct NewProfileCandidate {
    /// Source kind.
    pub source: ProfileCandidateSource,
    /// `session:<id>`, `page:<path>` or `stack:<tag>`.
    pub source_ref: String,
    /// Normalized topic tokens, space-separated.
    pub topic_key: String,
    /// Profile category.
    pub category: String,
    /// One-line statement.
    pub statement: String,
    /// The user's verbatim words (or the page line).
    pub quote: String,
    /// Stack tags the statement is about.
    pub applies_to: Vec<String>,
    /// When it was said (µs).
    pub observed_at: i64,
    /// Who said it, `None` when shared.
    pub contributor: Option<String>,
    /// Stated as general or project-scoped.
    pub generality: ProfileGenerality,
    /// Confidence in `[0, 1]`.
    pub confidence: f64,
}

/// A recorded candidate, with the project it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileCandidateRow {
    /// Workspace id.
    pub workspace_id: WorkspaceId,
    /// Project id.
    pub project_id: ProjectId,
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// The project is `restricted`.
    pub restricted: bool,
    /// The candidate itself.
    pub candidate: NewProfileCandidate,
}

/// A page of a profile scope, raw, for convergence and review.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileScopePage {
    /// Wiki path.
    pub path: String,
    /// Title.
    pub title: String,
    /// Body.
    pub body: String,
    /// Frontmatter.
    pub frontmatter: serde_json::Value,
}

fn is_restricted(mode: Option<&str>) -> bool {
    crate::AccessMode::from_db(mode) == crate::AccessMode::Restricted
}

/// Every contributing, non-reserved project with its harvest marks.
pub(crate) fn profile_harvest_projects(
    conn: &Connection,
) -> StoreResult<Vec<ProfileHarvestProject>> {
    let mut stmt = conn.prepare(
        "SELECT p.workspace_id, p.id, w.name, p.name, p.access_mode, \
                COALESCE(m.observations_until, 0), COALESCE(m.pages_until, 0) \
         FROM projects p JOIN workspaces w ON w.id = p.workspace_id \
         LEFT JOIN profile_harvest_marks m \
           ON m.workspace_id = p.workspace_id AND m.project_id = p.id \
         WHERE p.profile_contribute = 1 \
         ORDER BY w.name, p.name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;
    let mut projects = Vec::new();
    for row in rows {
        let (ws, proj, workspace, project, mode, observations_until, pages_until) = row?;
        // The reserved scopes hold the profile itself, and the cwd-less
        // catch-all project mixes unrelated work.
        if ai_memory_core::profile::is_reserved_scope_project(&project)
            || project == ai_memory_core::DEFAULT_PROJECT_NAME
        {
            continue;
        }
        let (Ok(workspace_id), Ok(project_id)) =
            (WorkspaceId::from_slice(&ws), ProjectId::from_slice(&proj))
        else {
            continue;
        };
        projects.push(ProfileHarvestProject {
            workspace_id,
            project_id,
            workspace,
            project,
            restricted: is_restricted(mode.as_deref()),
            mark: ProfileHarvestMark {
                observations_until,
                pages_until,
            },
        });
    }
    Ok(projects)
}

/// User prompts of a project captured after `since`, oldest first, at most
/// `limit`. Only `user-prompt` observations: tool output is never read.
pub(crate) fn profile_prompts_since(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    since: i64,
    limit: usize,
) -> StoreResult<Vec<ProfilePromptRow>> {
    let limit = i64::try_from(limit.clamp(1, PROFILE_HARVEST_BATCH)).unwrap_or(1);
    let mut stmt = conn.prepare_cached(
        "SELECT o.session_id, o.created_at, o.body, s.actor_user \
         FROM observations o LEFT JOIN sessions s ON s.id = o.session_id \
         WHERE o.workspace_id = ?1 AND o.project_id = ?2 AND o.kind = ?3 \
           AND o.created_at > ?4 \
         ORDER BY o.created_at ASC LIMIT ?5",
    )?;
    let rows = stmt.query_map(
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            ai_memory_core::ObservationKind::UserPrompt.as_str(),
            since,
            limit
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        },
    )?;
    let mut prompts = Vec::new();
    for row in rows {
        let (session, created_at, body, contributor) = row?;
        let Ok(session_id) = ai_memory_core::SessionId::from_slice(&session) else {
            continue;
        };
        prompts.push(ProfilePromptRow {
            session_id,
            created_at,
            body,
            contributor,
        });
    }
    Ok(prompts)
}

/// Curated pages of a project updated after `since`, oldest first, at most
/// `limit`.
pub(crate) fn profile_pages_since(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    since: i64,
    limit: usize,
) -> StoreResult<Vec<ProfilePageRow>> {
    let limit = i64::try_from(limit.clamp(1, PROFILE_HARVEST_BATCH)).unwrap_or(1);
    let prefixes = CURATED_PREFIXES
        .iter()
        .map(|prefix| format!("pages.path GLOB '{prefix}*'"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let sql = format!(
        "SELECT pages.path, pages.title, pages.body, pages.frontmatter_json, pages.updated_at, \
                u.username \
         FROM pages LEFT JOIN users u ON u.id = pages.author_id \
         WHERE pages.workspace_id = ?1 AND pages.project_id = ?2 AND pages.is_latest = 1 \
           AND pages.updated_at > ?3 AND ({prefixes}){not_expired} \
         ORDER BY pages.updated_at ASC LIMIT ?5",
        not_expired = not_expired("pages", "?4"),
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            since,
            now_us(),
            limit
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        },
    )?;
    let mut pages = Vec::new();
    for row in rows {
        let (path, title, body, frontmatter, updated_at, author) = row?;
        pages.push(ProfilePageRow {
            path,
            title,
            body,
            frontmatter: serde_json::from_str(&frontmatter).unwrap_or(serde_json::Value::Null),
            updated_at,
            contributor: author
                .map(|username| ai_memory_core::IdentityKey::User(username).storage_key()),
        });
    }
    Ok(pages)
}

/// The `agent` of each given proposal's decider
/// (`auto_improve_proposals.decided_by_actor_json`), keyed by proposal id, in
/// one batch lookup. The harvester reads it to tell a page the scheduler
/// auto-approved (model output nobody reviewed, #1211) from a page a human
/// approved. A proposal with no row — a legacy or imported page whose
/// frontmatter id resolves nowhere — is absent, and stays curated.
pub(crate) fn auto_improve_decided_by(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    proposal_ids: &[AutoImproveProposalId],
) -> StoreResult<HashMap<AutoImproveProposalId, Option<String>>> {
    if proposal_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let blobs: Vec<Value> = proposal_ids
        .iter()
        .map(|id| Value::Blob(id.as_bytes().to_vec()))
        .collect();
    let placeholders = std::iter::repeat_n("?", blobs.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT id, json_extract(decided_by_actor_json, '$.agent') \
         FROM auto_improve_proposals \
         WHERE workspace_id = ? AND project_id = ? AND id IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params_from_iter(
            std::iter::once(&Value::Blob(workspace_id.as_bytes().to_vec()))
                .chain(std::iter::once(&Value::Blob(
                    project_id.as_bytes().to_vec(),
                )))
                .chain(blobs.iter()),
        ),
        |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<String>>(1)?)),
    )?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, agent) = row?;
        let Ok(id) = AutoImproveProposalId::from_slice(&id) else {
            continue;
        };
        out.insert(id, agent);
    }
    Ok(out)
}

/// Record a project's candidates and advance its marks, in one transaction.
/// A candidate already recorded from the same source is ignored, and a mark
/// never moves backwards. Returns how many candidates were new.
pub(crate) fn record_profile_harvest(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    candidates: &[NewProfileCandidate],
    mark: ProfileHarvestMark,
) -> StoreResult<usize> {
    let tx = conn.transaction()?;
    let mut inserted = 0;
    {
        let mut insert = tx.prepare_cached(
            "INSERT OR IGNORE INTO profile_candidates \
             (workspace_id, project_id, source_kind, source_ref, topic_key, category, \
              statement, quote, applies_to, observed_at, contributor, generality, confidence) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )?;
        for candidate in candidates {
            inserted += insert.execute(params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                candidate.source.as_str(),
                candidate.source_ref,
                candidate.topic_key,
                candidate.category,
                candidate.statement,
                candidate.quote,
                serde_json::to_string(&candidate.applies_to).unwrap_or_else(|_| "[]".into()),
                candidate.observed_at,
                candidate.contributor,
                candidate.generality.as_str(),
                candidate.confidence,
            ])?;
        }
    }
    tx.execute(
        "INSERT INTO profile_harvest_marks \
         (workspace_id, project_id, observations_until, pages_until) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (workspace_id, project_id) DO UPDATE SET \
           observations_until = MAX(observations_until, excluded.observations_until), \
           pages_until = MAX(pages_until, excluded.pages_until)",
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            mark.observations_until,
            mark.pages_until
        ],
    )?;
    tx.commit()?;
    Ok(inserted)
}

/// Forget every harvest mark, so the next pass re-reads each project from the
/// start (`ai-memory profile rebuild`). Candidates stay: recording one again
/// is a no-op.
pub(crate) fn clear_profile_harvest_marks(conn: &Connection) -> StoreResult<usize> {
    Ok(conn.execute("DELETE FROM profile_harvest_marks", [])?)
}

/// The newest recorded candidates of projects that still contribute, at most
/// `limit`. A project that opted out after it was harvested drops out here.
pub(crate) fn profile_candidates(
    conn: &Connection,
    limit: usize,
) -> StoreResult<Vec<ProfileCandidateRow>> {
    let limit = i64::try_from(limit.clamp(1, PROFILE_CANDIDATES_LIMIT)).unwrap_or(1);
    let mut stmt = conn.prepare_cached(
        "SELECT c.workspace_id, c.project_id, w.name, p.name, p.access_mode, c.source_kind, \
                c.source_ref, c.topic_key, c.category, c.statement, c.quote, c.applies_to, \
                c.observed_at, c.contributor, c.generality, c.confidence \
         FROM profile_candidates c \
         JOIN projects p ON p.id = c.project_id \
         JOIN workspaces w ON w.id = c.workspace_id \
         WHERE p.profile_contribute = 1 \
         ORDER BY c.observed_at DESC, c.id DESC LIMIT ?1",
    )?;
    let mut rows = stmt.query(params![limit])?;
    let mut candidates = Vec::new();
    while let Some(row) = rows.next()? {
        let (Ok(workspace_id), Ok(project_id), Some(source)) = (
            WorkspaceId::from_slice(&row.get::<_, Vec<u8>>(0)?),
            ProjectId::from_slice(&row.get::<_, Vec<u8>>(1)?),
            ProfileCandidateSource::from_db(&row.get::<_, String>(5)?),
        ) else {
            continue;
        };
        let applies_to: String = row.get(11)?;
        candidates.push(ProfileCandidateRow {
            workspace_id,
            project_id,
            workspace: row.get(2)?,
            project: row.get(3)?,
            restricted: is_restricted(row.get::<_, Option<String>>(4)?.as_deref()),
            candidate: NewProfileCandidate {
                source,
                source_ref: row.get(6)?,
                topic_key: row.get(7)?,
                category: row.get(8)?,
                statement: row.get(9)?,
                quote: row.get(10)?,
                applies_to: serde_json::from_str(&applies_to).unwrap_or_default(),
                observed_at: row.get(12)?,
                contributor: row.get(13)?,
                generality: ProfileGenerality::from_db(&row.get::<_, String>(14)?),
                confidence: row.get(15)?,
            },
        });
    }
    Ok(candidates)
}

/// A profile page the harvester wrote: its path, topic and write time (µs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileLedgerEntry {
    /// Wiki path.
    pub path: String,
    /// Topic key the entry covers.
    pub topic_key: String,
    /// Last write (µs).
    pub written_at: i64,
}

/// The harvester's ledger for a profile scope.
pub(crate) fn profile_entry_ledger(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<Vec<ProfileLedgerEntry>> {
    let mut stmt = conn.prepare_cached(
        "SELECT path, topic_key, written_at FROM profile_entry_ledger \
         WHERE workspace_id = ?1 AND project_id = ?2 ORDER BY path",
    )?;
    let rows = stmt.query_map(
        params![workspace_id.as_bytes(), project_id.as_bytes()],
        |row| {
            Ok(ProfileLedgerEntry {
                path: row.get(0)?,
                topic_key: row.get(1)?,
                written_at: row.get(2)?,
            })
        },
    )?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Record that the harvester wrote `entries` into a profile scope.
pub(crate) fn record_profile_entries(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    entries: &[ProfileLedgerEntry],
) -> StoreResult<()> {
    let tx = conn.transaction()?;
    {
        let mut upsert = tx.prepare_cached(
            "INSERT INTO profile_entry_ledger \
             (workspace_id, project_id, path, topic_key, written_at) VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (workspace_id, project_id, path) DO UPDATE SET \
               topic_key = excluded.topic_key, written_at = excluded.written_at",
        )?;
        for entry in entries {
            upsert.execute(params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                entry.path,
                entry.topic_key,
                entry.written_at
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Every current, unexpired `profile/` page of a scope, raw.
pub(crate) fn profile_scope_pages(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<Vec<ProfileScopePage>> {
    let sql = format!(
        "SELECT path, title, body, frontmatter_json FROM pages \
         WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1 \
           AND path GLOB ?3{not_expired} \
         ORDER BY path ASC LIMIT ?5",
        not_expired = not_expired("pages", "?4"),
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            format!("{PROFILE_PATH_PREFIX}*"),
            now_us(),
            i64::try_from(PROFILE_ENTRIES_LIMIT).unwrap_or(i64::MAX)
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        },
    )?;
    let mut pages = Vec::new();
    for row in rows {
        let (path, title, body, frontmatter) = row?;
        pages.push(ProfileScopePage {
            path,
            title,
            body,
            frontmatter: serde_json::from_str(&frontmatter).unwrap_or(serde_json::Value::Null),
        });
    }
    Ok(pages)
}
