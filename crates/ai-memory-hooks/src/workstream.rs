//! Authenticated HTTP ingress for optional managed workstreams.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use ai_memory_core::{
    AgentKind, AuthLevel, Capability, FinishManagedRunRequest, FinishManagedRunResponse,
    LinkManagedRunRequest, ManagedRunContextResponse, ManagedRunId, ManagedRunStatus,
    NewWorkstreamEvent, PrepareManagedRunRequest, PrepareManagedRunResponse, Sanitizer,
    WorkstreamEventKind, WorkstreamId,
};
use ai_memory_store::{
    FinishWorkstreamRun, PrepareWorkstreamRun, ReaderPool, StoreError, WorkstreamSelection,
    WriterHandle, create_explicit_scope,
};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tracing::warn;

const MAX_EVENTS_PER_FINISH: usize = 4_096;
const MAX_EVENT_CONTENT_BYTES: usize = 64 * 1024;
const MAX_EVENT_ID_BYTES: usize = 512;
const MAX_NATIVE_SESSION_ID_BYTES: usize = 512;
const MAX_METADATA_BYTES: usize = 16 * 1024;
const MAX_NAME_BYTES: usize = 256;
const MAX_CWD_BYTES: usize = 16 * 1024;

/// State shared by the managed-workstream HTTP endpoints.
#[derive(Clone)]
pub struct WorkstreamState {
    /// Single-writer store actor.
    pub writer: WriterHandle,
    /// Read pool for status and packet assembly.
    pub reader: ReaderPool,
    /// Privacy scrubber applied before raw or indexed persistence.
    pub sanitizer: Sanitizer,
    /// ai-memory data root containing `raw/workstreams`.
    pub data_dir: PathBuf,
}

/// Build the host-wrapper API. It is mounted beside `/hook` and therefore
/// receives the same bearer-auth middleware as MCP and hook ingress.
pub fn workstream_router(state: WorkstreamState) -> Router {
    Router::new()
        .route("/workstream/runs", post(prepare_run))
        .route("/workstream/runs/{run_id}", get(run_status))
        .route("/workstream/runs/{run_id}/heartbeat", post(heartbeat_run))
        .route("/workstream/runs/{run_id}/cancel", post(cancel_run))
        .route("/workstream/runs/{run_id}/context", post(run_context))
        .route(
            "/workstream/runs/{run_id}/context/accept",
            post(accept_run_context),
        )
        .route("/workstream/runs/{run_id}/link", post(link_run))
        .route("/workstream/runs/{run_id}/finish", post(finish_run))
        .route("/workstream/{workstream_id}/events", get(search_events))
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
}

type ApiFailure = (StatusCode, Json<ApiError>);

fn api_failure(status: StatusCode, message: impl Into<String>) -> ApiFailure {
    (
        status,
        Json(ApiError {
            error: message.into(),
        }),
    )
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    api_failure(status, message).into_response()
}

fn authorize(
    level: Option<Extension<AuthLevel>>,
    capability: Capability,
) -> Result<(), ApiFailure> {
    let level = level.map_or(AuthLevel::Anonymous, |Extension(level)| level);
    level.authorize(capability, true).map_err(|failure| {
        let status = if failure.is_authentication_required() {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::FORBIDDEN
        };
        api_failure(status, failure.message())
    })
}

async fn prepare_run(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    Json(request): Json<PrepareManagedRunRequest>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    if request.workspace.trim().is_empty()
        || request.project.trim().is_empty()
        || request.cwd.trim().is_empty()
        || request.repo_fingerprint.trim().is_empty()
        || request.worktree_fingerprint.trim().is_empty()
        || request.lease_owner.trim().is_empty()
    {
        return error(
            StatusCode::BAD_REQUEST,
            "managed run fields cannot be empty",
        );
    }
    if request.cwd.len() > MAX_CWD_BYTES {
        return error(StatusCode::BAD_REQUEST, "managed run cwd is too long");
    }
    if request.workstream.is_some() && request.new_workstream.is_some() {
        return error(
            StatusCode::BAD_REQUEST,
            "workstream and new_workstream are mutually exclusive",
        );
    }
    if !matches!(
        request.agent,
        AgentKind::ClaudeCode
            | AgentKind::Codex
            | AgentKind::OpenCode
            | AgentKind::Pi
            | AgentKind::Crush
            | AgentKind::Omp
            | AgentKind::KimiCode
    ) {
        return error(
            StatusCode::BAD_REQUEST,
            "managed run requires a supported command-line harness",
        );
    }
    const AUTO_AGENTS: [AgentKind; 6] = [
        AgentKind::ClaudeCode,
        AgentKind::Codex,
        AgentKind::OpenCode,
        AgentKind::Pi,
        AgentKind::Crush,
        AgentKind::KimiCode,
    ];
    if request.automatic_harness
        && (!AUTO_AGENTS.contains(&request.agent)
            || !request.available_agents.contains(&request.agent)
            || request
                .available_agents
                .iter()
                .any(|agent| !AUTO_AGENTS.contains(agent)))
    {
        return error(
            StatusCode::BAD_REQUEST,
            "automatic managed run requires supported checkout-local harnesses",
        );
    }
    for (label, value) in [
        ("workspace", request.workspace.as_str()),
        ("project", request.project.as_str()),
        ("repo_fingerprint", request.repo_fingerprint.as_str()),
        (
            "worktree_fingerprint",
            request.worktree_fingerprint.as_str(),
        ),
        ("lease_owner", request.lease_owner.as_str()),
    ] {
        if value.len() > MAX_NAME_BYTES {
            return error(StatusCode::BAD_REQUEST, format!("{label} is too long"));
        }
    }
    let scope = match create_explicit_scope(
        &state.writer,
        request.workspace.trim(),
        request.project.trim(),
    )
    .await
    {
        Ok(scope) => scope,
        Err(failure) => return error(StatusCode::BAD_REQUEST, failure.to_string()),
    };
    let selection = match (request.workstream, request.new_workstream) {
        (Some(name), None) => WorkstreamSelection::Named(name.trim().to_string()),
        (None, Some(name)) => WorkstreamSelection::New(name.trim().to_string()),
        (None, None) => WorkstreamSelection::Current,
        (Some(_), Some(_)) => {
            return error(
                StatusCode::BAD_REQUEST,
                "workstream and new_workstream are mutually exclusive",
            );
        }
    };
    let prepared = state
        .writer
        .prepare_workstream_run(PrepareWorkstreamRun {
            workspace_id: scope.workspace_id,
            project_id: scope.project_id,
            repo_fingerprint: request.repo_fingerprint,
            worktree_fingerprint: request.worktree_fingerprint,
            cwd: request.cwd,
            agent: request.agent,
            automatic_harness: request.automatic_harness,
            available_agents: request.available_agents,
            selection,
            lease_owner: request.lease_owner,
        })
        .await;
    match prepared {
        Ok(prepared) => Json(PrepareManagedRunResponse {
            workstream_id: prepared.workstream_id,
            workstream_name: prepared.workstream_name,
            run_id: prepared.run_id,
            resolved_agent: Some(prepared.agent),
            native_session_id: prepared.native_session_id,
            source_cursor: prepared.source_cursor,
            sync_after: prepared.sync_after,
            sync_through: prepared.sync_through,
            may_adopt_existing_session: prepared.may_adopt_existing_session,
        })
        .into_response(),
        Err(failure) => store_error_response(failure),
    }
}

async fn run_status(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    AxumPath(raw_run_id): AxumPath<String>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalRead) {
        return response.into_response();
    }
    let run_id = match parse_run_id(&raw_run_id) {
        Ok(id) => id,
        Err(response) => return response.into_response(),
    };
    match state.reader.managed_run_status(run_id).await {
        Ok(Some(status)) => Json(ManagedRunStatus {
            run_id: status.run_id,
            workstream_id: status.workstream_id,
            agent: status.agent,
            native_session_id: status.native_session_id,
            context_delivered: status.context_delivered,
            state: status.state,
        })
        .into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "managed run not found"),
        Err(failure) => error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    }
}

async fn heartbeat_run(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    AxumPath(raw_run_id): AxumPath<String>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    let run_id = match parse_run_id(&raw_run_id) {
        Ok(id) => id,
        Err(response) => return response.into_response(),
    };
    match state.writer.heartbeat_managed_run(run_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(StatusCode::CONFLICT, "managed run lease is not active"),
        Err(failure) => store_error_response(failure),
    }
}

async fn cancel_run(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    AxumPath(raw_run_id): AxumPath<String>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    let run_id = match parse_run_id(&raw_run_id) {
        Ok(id) => id,
        Err(response) => return response.into_response(),
    };
    match state.writer.cancel_managed_run(run_id).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(failure) => store_error_response(failure),
    }
}

async fn run_context(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    AxumPath(raw_run_id): AxumPath<String>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    let run_id = match parse_run_id(&raw_run_id) {
        Ok(id) => id,
        Err(response) => return response.into_response(),
    };
    let context = match state.reader.managed_run_context(run_id, 256).await {
        Ok(Some(context)) => context,
        Ok(None) => return error(StatusCode::NOT_FOUND, "active managed run not found"),
        Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    };
    if context.agent != AgentKind::Crush {
        return error(
            StatusCode::BAD_REQUEST,
            "direct managed context is only supported for Crush",
        );
    }
    if context.context_delivered {
        return Json(ManagedRunContextResponse { context: None }).into_response();
    }
    let rendered = crate::router::render_managed_context(
        &context.events,
        &context.workstream_name,
        context.workstream_id,
        context.sync_after,
    );
    Json(ManagedRunContextResponse { context: rendered }).into_response()
}

async fn accept_run_context(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    AxumPath(raw_run_id): AxumPath<String>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    let run_id = match parse_run_id(&raw_run_id) {
        Ok(id) => id,
        Err(response) => return response.into_response(),
    };
    let status = match state.reader.managed_run_status(run_id).await {
        Ok(Some(status)) => status,
        Ok(None) => return error(StatusCode::NOT_FOUND, "managed run not found"),
        Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    };
    if status.agent != AgentKind::Crush {
        return error(
            StatusCode::BAD_REQUEST,
            "direct managed context is only supported for Crush",
        );
    }
    match state.writer.accept_managed_run_context(run_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(StatusCode::CONFLICT, "managed run lease is not active"),
        Err(failure) => store_error_response(failure),
    }
}

#[derive(Debug, Default, Deserialize)]
struct EventQuery {
    #[serde(default)]
    q: String,
    #[serde(default = "default_event_limit")]
    limit: usize,
}

const fn default_event_limit() -> usize {
    20
}

async fn search_events(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    AxumPath(raw_workstream_id): AxumPath<String>,
    Query(query): Query<EventQuery>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalRead) {
        return response.into_response();
    }
    let workstream_id = match WorkstreamId::from_str(&raw_workstream_id) {
        Ok(id) => id,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid workstream id"),
    };
    match state
        .reader
        .search_workstream_events(workstream_id, query.q, query.limit.clamp(1, 100))
        .await
    {
        Ok(events) => Json(events).into_response(),
        Err(failure) => error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    }
}

async fn link_run(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    AxumPath(raw_run_id): AxumPath<String>,
    Json(request): Json<LinkManagedRunRequest>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    let run_id = match parse_run_id(&raw_run_id) {
        Ok(id) => id,
        Err(response) => return response.into_response(),
    };
    if request.native_session_id.trim().is_empty()
        || request.native_session_id.len() > MAX_NATIVE_SESSION_ID_BYTES
    {
        return error(StatusCode::BAD_REQUEST, "invalid native session id");
    }
    let status = match state.reader.managed_run_status(run_id).await {
        Ok(Some(status)) => status,
        Ok(None) => return error(StatusCode::NOT_FOUND, "managed run not found"),
        Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    };
    if status.state != "active" {
        return error(StatusCode::CONFLICT, "managed run is not active");
    }
    match state
        .writer
        .link_managed_run_session(run_id, status.agent, request.native_session_id)
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(StatusCode::CONFLICT, "managed run is not active"),
        Err(failure) => store_error_response(failure),
    }
}

async fn finish_run(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    AxumPath(raw_run_id): AxumPath<String>,
    Json(mut request): Json<FinishManagedRunRequest>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    let run_id = match parse_run_id(&raw_run_id) {
        Ok(id) => id,
        Err(response) => return response.into_response(),
    };
    if request.events.len() > MAX_EVENTS_PER_FINISH {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("at most {MAX_EVENTS_PER_FINISH} events are accepted per finish"),
        );
    }
    if request
        .native_session_id
        .as_deref()
        .is_some_and(|id| id.trim().is_empty() || id.len() > MAX_NATIVE_SESSION_ID_BYTES)
    {
        return error(StatusCode::BAD_REQUEST, "invalid native session id");
    }
    let status = match state.reader.managed_run_status(run_id).await {
        Ok(Some(status)) => status,
        Ok(None) => return error(StatusCode::NOT_FOUND, "managed run not found"),
        Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    };
    if status.state == "finished" {
        return match state
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id,
                native_session_id: None,
                source_cursor: None,
                events: Vec::new(),
                complete: true,
                segment_path: None,
                exit_code: None,
            })
            .await
        {
            Ok(result) => Json(FinishManagedRunResponse {
                imported_events: 0,
                latest_sequence: result.latest_sequence,
            })
            .into_response(),
            Err(failure) => store_error_response(failure),
        };
    }
    if status.state != "active" {
        return error(StatusCode::CONFLICT, "managed run is not active");
    }
    let native_session_id = request
        .native_session_id
        .clone()
        .or(status.native_session_id.clone())
        .unwrap_or_else(|| format!("unresolved:{run_id}"));
    if let Err(message) = sanitize_events(
        &state.sanitizer,
        status.agent,
        &native_session_id,
        &mut request.events,
    ) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    if request.complete {
        append_boundary_events(
            &state.sanitizer,
            run_id,
            status.agent,
            &native_session_id,
            &mut request,
        );
    }
    let segment_path = match write_segment(
        &state.data_dir,
        status.workstream_id,
        run_id,
        &request.events,
    ) {
        Ok(path) => path,
        Err(failure) => {
            warn!(error = %failure, run = %run_id, "managed transcript segment write failed");
            return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string());
        }
    };
    let input = FinishWorkstreamRun {
        run_id,
        native_session_id: request.native_session_id.or(status.native_session_id),
        source_cursor: request.source_cursor,
        events: request.events,
        complete: request.complete,
        segment_path: Some(segment_path),
        exit_code: request.exit_code,
    };
    match state.writer.finish_workstream_run(input).await {
        Ok(result) => Json(FinishManagedRunResponse {
            imported_events: result.imported_events,
            latest_sequence: result.latest_sequence,
        })
        .into_response(),
        Err(failure) => store_error_response(failure),
    }
}

fn parse_run_id(raw: &str) -> Result<ManagedRunId, ApiFailure> {
    ManagedRunId::from_str(raw).map_err(|_| api_failure(StatusCode::BAD_REQUEST, "invalid run id"))
}

fn store_error_response(failure: StoreError) -> Response {
    let status = match failure {
        StoreError::WorkstreamBusy(_) | StoreError::Duplicate(_) => StatusCode::CONFLICT,
        StoreError::NotFound(_) => StatusCode::NOT_FOUND,
        StoreError::InvalidState(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error(status, failure.to_string())
}

fn sanitize_events(
    sanitizer: &Sanitizer,
    expected_agent: AgentKind,
    expected_session: &str,
    events: &mut [NewWorkstreamEvent],
) -> Result<(), String> {
    for event in events {
        if event.agent != expected_agent {
            return Err(format!(
                "event {} agent does not match managed run",
                event.event_id
            ));
        }
        if event.native_session_id != expected_session {
            return Err(format!(
                "event {} native session does not match managed run",
                event.event_id
            ));
        }
        if event.event_id.trim().is_empty() || event.event_id.len() > MAX_EVENT_ID_BYTES {
            return Err("invalid workstream event id".to_string());
        }
        if event.content.len() > MAX_EVENT_CONTENT_BYTES {
            let mut end = MAX_EVENT_CONTENT_BYTES;
            while !event.content.is_char_boundary(end) {
                end -= 1;
            }
            event.content.truncate(end);
            event.content.push_str("\n[truncated by ai-memory]");
        }
        event.content = sanitizer.scrub(&event.content);
        if let Some(role) = &event.role
            && (role.len() > 32
                || !role
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')))
        {
            return Err("invalid workstream message role".to_string());
        }
        let raw_metadata = serde_json::to_string(&event.metadata).map_err(|e| e.to_string())?;
        if raw_metadata.len() > MAX_METADATA_BYTES {
            event.metadata = serde_json::json!({ "truncated": true });
        } else {
            let scrubbed = sanitizer.scrub(&raw_metadata);
            event.metadata = serde_json::from_str(&scrubbed)
                .unwrap_or_else(|_| serde_json::json!({ "redacted": true }));
        }
    }
    Ok(())
}

fn append_boundary_events(
    sanitizer: &Sanitizer,
    run_id: ManagedRunId,
    agent: AgentKind,
    native_session_id: &str,
    request: &mut FinishManagedRunRequest,
) {
    let mut checkpoint = String::new();
    if let Some(head) = &request.checkpoint.head {
        let _ = writeln!(checkpoint, "HEAD: {head}");
    }
    if let Some(branch) = &request.checkpoint.branch {
        let _ = writeln!(checkpoint, "Branch: {branch}");
    }
    if let Some(dirty_hash) = &request.checkpoint.dirty_hash {
        let _ = writeln!(checkpoint, "Dirty-state hash: {dirty_hash}");
    }
    if !request.checkpoint.changed_paths.is_empty() {
        checkpoint.push_str("Changed paths:\n");
        for path in &request.checkpoint.changed_paths {
            let _ = writeln!(checkpoint, "- {path}");
        }
    }
    if checkpoint.is_empty() {
        checkpoint.push_str("No Git repository checkpoint was available.");
    }
    truncate_owned(&mut checkpoint, MAX_EVENT_CONTENT_BYTES);
    request.events.push(NewWorkstreamEvent {
        event_id: format!("managed-run:{run_id}:checkpoint"),
        agent,
        native_session_id: native_session_id.to_string(),
        source_record_id: None,
        kind: WorkstreamEventKind::Checkpoint,
        role: None,
        content: sanitizer.scrub(&checkpoint),
        occurred_at: None,
        metadata: serde_json::json!({ "exit_code": request.exit_code }),
    });
    if !request.losses.is_empty() {
        let mut content = String::from("Transcript extraction losses:\n");
        for loss in &request.losses {
            let _ = writeln!(content, "- {loss}");
        }
        truncate_owned(&mut content, MAX_EVENT_CONTENT_BYTES);
        request.events.push(NewWorkstreamEvent {
            event_id: format!("managed-run:{run_id}:losses"),
            agent,
            native_session_id: native_session_id.to_string(),
            source_record_id: None,
            kind: WorkstreamEventKind::Annotation,
            role: None,
            content: sanitizer.scrub(&content),
            occurred_at: None,
            metadata: serde_json::json!({ "loss_count": request.losses.len() }),
        });
    }
}

fn write_segment(
    data_dir: &Path,
    workstream_id: ai_memory_core::WorkstreamId,
    run_id: ManagedRunId,
    events: &[NewWorkstreamEvent],
) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    for event in events {
        serde_json::to_writer(&mut bytes, event).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
    }
    let digest = Sha256::digest(&bytes);
    let digest_hex = format!("{digest:x}");
    let relative = PathBuf::from("workstreams")
        .join(workstream_id.to_string())
        .join("segments")
        .join(format!("{run_id}-{}.jsonl", &digest_hex[..16]));
    let target = data_dir.join("raw").join(&relative);
    let parent = target
        .parent()
        .ok_or_else(|| std::io::Error::other("managed segment has no parent"))?;
    std::fs::create_dir_all(parent)?;
    if target.exists() {
        return Ok(relative.to_string_lossy().replace('\\', "/"));
    }
    let temp = parent.join(format!(".{run_id}-{}.tmp", ManagedRunId::new()));
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    if let Err(failure) = std::fs::rename(&temp, &target) {
        let target_won_race = target.exists();
        let _ = std::fs::remove_file(&temp);
        if !target_won_race {
            return Err(failure);
        }
    }
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn truncate_owned(value: &mut String, max: usize) {
    if value.len() <= max {
        return;
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push_str("\n[truncated by ai-memory]");
}

#[cfg(test)]
mod tests {
    use ai_memory_store::Store;
    use axum::body::to_bytes;
    use tempfile::TempDir;

    use super::*;

    fn test_state(store: &Store, data_dir: &Path) -> WorkstreamState {
        WorkstreamState {
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            sanitizer: Sanitizer::default(),
            data_dir: data_dir.to_path_buf(),
        }
    }

    fn prepare_input(
        workspace_id: ai_memory_core::WorkspaceId,
        project_id: ai_memory_core::ProjectId,
        agent: AgentKind,
        owner: &str,
    ) -> PrepareWorkstreamRun {
        PrepareWorkstreamRun {
            workspace_id,
            project_id,
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            cwd: "/repo".into(),
            agent,
            automatic_harness: false,
            available_agents: Vec::new(),
            selection: WorkstreamSelection::Current,
            lease_owner: owner.into(),
        }
    }

    async fn seed_scope(store: &Store) -> (ai_memory_core::WorkspaceId, ai_memory_core::ProjectId) {
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
        (workspace_id, project_id)
    }

    #[tokio::test]
    async fn automatic_prepare_returns_the_established_available_harness() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let (workspace_id, project_id) = seed_scope(&store).await;
        let claude = store
            .writer
            .prepare_workstream_run(prepare_input(
                workspace_id,
                project_id,
                AgentKind::ClaudeCode,
                "claude",
            ))
            .await
            .unwrap();
        store
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id: claude.run_id,
                native_session_id: Some("claude-current".into()),
                source_cursor: Some("cursor".into()),
                events: Vec::new(),
                complete: true,
                segment_path: None,
                exit_code: Some(0),
            })
            .await
            .unwrap();

        let response = prepare_run(
            State(state),
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::Codex,
                automatic_harness: true,
                available_agents: vec![AgentKind::Codex, AgentKind::ClaudeCode],
                workstream: None,
                new_workstream: None,
                lease_owner: "automatic".into(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let prepared: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(prepared.resolved_agent, Some(AgentKind::ClaudeCode));
        assert_eq!(
            prepared.native_session_id.as_deref(),
            Some("claude-current")
        );
        assert!(!prepared.may_adopt_existing_session);
    }

    #[tokio::test]
    async fn kimi_code_is_accepted_as_an_explicit_and_automatic_harness() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());

        let explicit = prepare_run(
            State(state.clone()),
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::KimiCode,
                automatic_harness: false,
                available_agents: Vec::new(),
                workstream: None,
                new_workstream: None,
                lease_owner: "explicit".into(),
            }),
        )
        .await;
        assert_eq!(explicit.status(), StatusCode::OK);
        let body = to_bytes(explicit.into_body(), 64 * 1024).await.unwrap();
        let prepared: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(prepared.resolved_agent, Some(AgentKind::KimiCode));
        // Close the explicit run so the automatic prepare can take the lease.
        store
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id: prepared.run_id,
                native_session_id: Some("session_abc".into()),
                source_cursor: None,
                events: Vec::new(),
                complete: true,
                segment_path: None,
                exit_code: Some(0),
            })
            .await
            .unwrap();

        let automatic = prepare_run(
            State(state),
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::KimiCode,
                automatic_harness: true,
                available_agents: vec![AgentKind::KimiCode, AgentKind::ClaudeCode],
                workstream: None,
                new_workstream: None,
                lease_owner: "automatic".into(),
            }),
        )
        .await;
        assert_eq!(automatic.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn cancel_endpoint_is_idempotent_and_releases_the_workstream() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let (workspace_id, project_id) = seed_scope(&store).await;
        let input = prepare_input(workspace_id, project_id, AgentKind::Codex, "launcher");
        let prepared = store
            .writer
            .prepare_workstream_run(input.clone())
            .await
            .unwrap();

        for _ in 0..2 {
            let response = cancel_run(
                State(state.clone()),
                None,
                AxumPath(prepared.run_id.to_string()),
            )
            .await;
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }
        store.writer.prepare_workstream_run(input).await.unwrap();
    }

    #[tokio::test]
    async fn crush_context_fetch_is_repeatable_until_explicit_accept() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let (workspace_id, project_id) = seed_scope(&store).await;
        let codex = store
            .writer
            .prepare_workstream_run(prepare_input(
                workspace_id,
                project_id,
                AgentKind::Codex,
                "codex",
            ))
            .await
            .unwrap();
        store
            .writer
            .finish_workstream_run(FinishWorkstreamRun {
                run_id: codex.run_id,
                native_session_id: Some("codex-native".into()),
                source_cursor: None,
                events: vec![NewWorkstreamEvent {
                    event_id: "codex:assistant:1".into(),
                    agent: AgentKind::Codex,
                    native_session_id: "codex-native".into(),
                    source_record_id: None,
                    kind: WorkstreamEventKind::Message,
                    role: Some("assistant".into()),
                    content: "AMWS-CODEX-SENTINEL".into(),
                    occurred_at: None,
                    metadata: serde_json::json!({}),
                }],
                complete: true,
                segment_path: None,
                exit_code: Some(0),
            })
            .await
            .unwrap();
        let crush = store
            .writer
            .prepare_workstream_run(prepare_input(
                workspace_id,
                project_id,
                AgentKind::Crush,
                "crush",
            ))
            .await
            .unwrap();

        let fetch = || {
            run_context(
                State(state.clone()),
                None,
                AxumPath(crush.run_id.to_string()),
            )
        };
        for _ in 0..2 {
            let response = fetch().await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            let packet: ManagedRunContextResponse = serde_json::from_slice(&body).unwrap();
            assert!(
                packet
                    .context
                    .as_deref()
                    .is_some_and(|context| context.contains("AMWS-CODEX-SENTINEL"))
            );
            assert!(
                !store
                    .reader
                    .managed_run_status(crush.run_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .context_delivered
            );
        }

        let accepted = accept_run_context(
            State(state.clone()),
            None,
            AxumPath(crush.run_id.to_string()),
        )
        .await;
        assert_eq!(accepted.status(), StatusCode::NO_CONTENT);
        assert!(
            store
                .reader
                .managed_run_status(crush.run_id)
                .await
                .unwrap()
                .unwrap()
                .context_delivered
        );

        let response = fetch().await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let packet: ManagedRunContextResponse = serde_json::from_slice(&body).unwrap();
        assert!(packet.context.is_none());
    }
}
