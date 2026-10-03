//! Bounded native session inspection with ephemeral request authorization.
use crate::{
    ObservationRecord, ProjectAccess, ProjectAuthz, ProjectPrincipal, ReaderPool, StoreResult,
};
use ai_memory_core::page::NativeSessionEvidence;
use ai_memory_core::{ActorContext, AuthLevel, OwnerFilter, ProjectId, UserId, WorkspaceId};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

const HEADER_BYTES: i64 = 256 * 1024;
const RECORD_BYTES: i64 = 4096;
const PREFIX_BYTES: i64 = 32 * 1024;

/// Trusted request identity, separate from attribution and source declarations.
#[derive(Clone, Debug)]
pub struct SourceAuthorization {
    principal: ProjectPrincipal,
    owner: OwnerFilter,
    authenticated: bool,
}
impl SourceAuthorization {
    /// Build only from auth-middleware extensions, never raw request headers.
    pub fn from_auth(level: AuthLevel, user: Option<UserId>, actor: &ActorContext) -> Self {
        let principal = match level {
            AuthLevel::Root => ProjectPrincipal::root(),
            AuthLevel::User => {
                user.map_or_else(ProjectPrincipal::anonymous, ProjectPrincipal::user)
            }
            AuthLevel::Anonymous => ProjectPrincipal::anonymous(),
        };
        let owner = match level {
            AuthLevel::Root => OwnerFilter::Unattributed,
            AuthLevel::User => OwnerFilter::for_actor_context(actor),
            AuthLevel::Anonymous => OwnerFilter::Unattributed,
        };
        Self {
            principal,
            owner,
            authenticated: level == AuthLevel::Root
                || (level == AuthLevel::User && user.is_some() && actor.identity_key().is_some()),
        }
    }
}

/// Opaque bounded capture consumed by the final read-pool recheck. No wire form.
pub struct NativeSessionRead {
    ws: WorkspaceId,
    pj: ProjectId,
    source: NativeSessionEvidence,
    caller: SourceAuthorization,
    snapshot: SourceSnapshot,
    records: Vec<ObservationRecord>,
}
#[derive(Debug, PartialEq, Eq)]
struct SourceSnapshot {
    highwater: i64,
    count: usize,
    digest: Vec<u8>,
}
fn unavailable() -> crate::StoreError {
    ai_memory_core::MemoryError::MalformedRecord("native source unavailable".into()).into()
}

// Native inspection fails closed; legacy project resolution remains unchanged.
fn authorize(
    conn: &Connection,
    ws: WorkspaceId,
    pj: ProjectId,
    caller: &SourceAuthorization,
) -> StoreResult<bool> {
    if !caller.authenticated {
        return Ok(false);
    }
    let row: Option<(String, Option<Vec<u8>>)> = conn
        .query_row(
            "SELECT access_mode, created_by FROM projects WHERE workspace_id=?1 AND id=?2",
            params![ws.as_bytes(), pj.as_bytes()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((mode, creator)) = row else {
        return Ok(false);
    };
    let mode = match mode.as_str() {
        "open" => crate::AccessMode::Open,
        "restricted" => crate::AccessMode::Restricted,
        _ => return Err(unavailable()),
    };
    let grant: Option<String> = conn.query_row(
        "SELECT level FROM project_grants WHERE workspace_id=?1 AND project_id=?2 AND user_id=?3",
        params![ws.as_bytes(), pj.as_bytes(), caller.principal.user_id.map(|u| *u.as_bytes())], |r| r.get(0),
    ).optional()?;
    Ok(ProjectAuthz {
        distinguishes_operators: true,
        access_mode: mode,
        is_root: caller.principal.is_root,
        is_creator: creator
            .as_deref()
            .is_some_and(|c| caller.principal.user_id.is_some_and(|u| u.as_bytes() == c)),
        grant: grant.as_deref().and_then(crate::GrantLevel::from_db),
    }
    .authorize(ProjectAccess::Read)
    .is_ok())
}

fn source_digest(
    conn: &Connection,
    ws: WorkspaceId,
    pj: ProjectId,
    source: &NativeSessionEvidence,
    caller: &SourceAuthorization,
    highwater: i64,
) -> StoreResult<SourceSnapshot> {
    let sid = source.session_id()?;
    let purged: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM purged_sessions WHERE session_id=?1 AND workspace_id=?2 AND project_id=?3) OR EXISTS(SELECT 1 FROM purged_scopes WHERE workspace_id=?2 AND (project_id=?3 OR project_id=zeroblob(16)))",
        params![sid.as_bytes(), ws.as_bytes(), pj.as_bytes()], |r| r.get(0),
    )?;
    if purged {
        return Err(unavailable());
    }
    // Check raw column bytes before JSON construction or record hydration.
    let header: Option<i64> = conn.query_row(
        "SELECT length(CAST(agent_kind AS BLOB))+COALESCE(length(CAST(cwd AS BLOB)),0)+COALESCE(length(CAST(actor_user AS BLOB)),0)+length(CAST(started_at AS BLOB))
         FROM sessions WHERE id=?1 AND workspace_id=?2 AND project_id=?3",
        params![sid.as_bytes(), ws.as_bytes(), pj.as_bytes()], |r| r.get(0),
    ).optional()?;
    if header.is_none_or(|bytes| bytes > HEADER_BYTES) {
        return Err(unavailable());
    }
    let (owner, data): (Option<String>, String) = conn.query_row(
        "SELECT actor_user,json_array(agent_kind,cwd,started_at,actor_user) FROM sessions WHERE id=?1 AND workspace_id=?2 AND project_id=?3",
        params![sid.as_bytes(), ws.as_bytes(), pj.as_bytes()], |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if !caller.owner.admits(owner.as_deref()) {
        return Err(unavailable());
    }
    let (count, largest, bytes): (i64, i64, i64) = conn.query_row(
        "SELECT COUNT(*),COALESCE(MAX(bytes),0),COALESCE(SUM(bytes),0) FROM
         (SELECT length(CAST(id AS BLOB))+length(CAST(session_id AS BLOB))+length(CAST(kind AS BLOB))+length(CAST(title AS BLOB))+length(CAST(body AS BLOB))+length(CAST(importance AS BLOB))+length(CAST(created_at AS BLOB))+
          COALESCE(length(CAST(extension AS BLOB)),0)+COALESCE(length(CAST(source_event AS BLOB)),0) AS bytes
          FROM observations WHERE session_id=?1 AND workspace_id=?2 AND project_id=?3 AND rowid<=?4 ORDER BY rowid LIMIT 201)",
        params![sid.as_bytes(), ws.as_bytes(), pj.as_bytes(), highwater], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if count > 200 || largest > RECORD_BYTES || bytes > PREFIX_BYTES {
        return Err(unavailable());
    }
    let mut digest = Sha256::new();
    digest.update(data.as_bytes());
    let mut stmt = conn.prepare_cached("SELECT json_array(rowid,hex(id),kind,title,body,importance,created_at,extension,source_event) FROM observations WHERE session_id=?1 AND workspace_id=?2 AND project_id=?3 AND rowid<=?4 ORDER BY rowid")?;
    for row in stmt.query_map(
        params![sid.as_bytes(), ws.as_bytes(), pj.as_bytes(), highwater],
        |r| r.get::<_, String>(0),
    )? {
        digest.update(row?.as_bytes());
    }
    Ok(SourceSnapshot {
        highwater,
        count: count as usize,
        digest: digest.finalize().to_vec(),
    })
}

impl ReaderPool {
    /// Capture the beginning of an exact-origin session, at most 200 rows.
    ///
    /// # Errors
    /// Denied, absent, private or oversized sources are unavailable. SQL errors propagate.
    pub async fn capture_native_session(
        &self,
        ws: WorkspaceId,
        pj: ProjectId,
        source: NativeSessionEvidence,
        caller: SourceAuthorization,
        limit: usize,
    ) -> StoreResult<NativeSessionRead> {
        let sid = source.session_id()?;
        #[cfg(test)]
        let probe = self.native_capture_read_probe.clone();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            if !authorize(&tx, ws, pj, &caller)? {
                return Err(unavailable());
            }
            let highwater = tx.query_row(
                "SELECT COALESCE(MAX(rowid),0) FROM (SELECT rowid FROM observations WHERE session_id=?1 AND workspace_id=?2 AND project_id=?3 ORDER BY rowid LIMIT ?4)",
                params![sid.as_bytes(), ws.as_bytes(), pj.as_bytes(), limit.clamp(1, 200) as i64], |r| r.get(0),
            )?;
            let snapshot = source_digest(&tx, ws, pj, &source, &caller, highwater)?;
            #[cfg(test)]
            if let Some(probe) = &probe {
                probe();
            }
            let mut stmt = tx.prepare_cached("SELECT id,session_id,kind,title,body,importance,created_at,extension,source_event FROM observations WHERE session_id=?1 AND workspace_id=?2 AND project_id=?3 AND rowid<=?4 ORDER BY rowid")?;
            let records = stmt.query_map(params![sid.as_bytes(), ws.as_bytes(), pj.as_bytes(), highwater], crate::reader::row_to_observation_record)?
                .map(|r| r?).collect::<StoreResult<_>>()?;
            Ok(NativeSessionRead { ws, pj, source, caller, snapshot, records })
        }).await
    }

    /// Consume a capture only after rechecking current authority and the same prefix.
    ///
    /// # Errors
    /// Refuses changed/deleted prefixes, owners, origins or revoked grants; propagates SQL errors.
    pub async fn revalidate_native_session(
        &self,
        read: NativeSessionRead,
    ) -> StoreResult<Vec<ObservationRecord>> {
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            if !authorize(&tx, read.ws, read.pj, &read.caller)?
                || source_digest(
                    &tx,
                    read.ws,
                    read.pj,
                    &read.source,
                    &read.caller,
                    read.snapshot.highwater,
                )? != read.snapshot
            {
                return Err(unavailable());
            }
            Ok(read.records)
        })
        .await
    }
}
