use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use ai_memory_client::sanitize_external_text;
use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use walkdir::WalkDir;

mod agentmemory;
mod claude_mem;
mod document;
mod mcp;
#[cfg(test)]
mod test_http;

const IMPORT_VERSION: &str = "omc-wiki-v1";
const CONVERSATION_IMPORT_VERSION: &str = "external-conversation-v1";
const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:49374";
const EXTERNAL_IMPORT_AGENT: &str = "external-import";
const EXTERNAL_IMPORT_EXTENSION: &str = "ai-memory-importer";
const MAX_CONVERSATION_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_CONVERSATION_MESSAGES: usize = 128;
const MAX_USER_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_EXTENSION_MESSAGE_BYTES: usize = 2_000;
const MAX_CONVERSATION_TOTAL_BYTES: usize = 1024 * 1024;

#[derive(Parser, Debug)]
#[command(author, version, about = "Optional ai-memory import companion")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Import an oh-my-claudecode / OMC flat markdown wiki directory.
    OmcWiki(OmcArgs),
    /// Replay a generic external conversation through ai-memory's hook API.
    ExternalConversation(ConversationArgs),
    /// Import the Obsidian vault agentmemory's `obsidian-export` writes.
    AgentmemoryVault(agentmemory::vault::VaultArgs),
    /// Import an agentmemory `ExportData` JSON file.
    AgentmemoryExport(agentmemory::export::ExportArgs),
    /// Import observations and session summaries from claude-mem's SQLite store.
    ClaudeMem(claude_mem::ClaudeMemArgs),
    /// Extract one bounded local document into an attributed wiki page.
    Document(document::DocumentArgs),
}

#[derive(Parser, Debug, Clone)]
struct OmcArgs {
    #[arg(long)]
    dir: PathBuf,
    #[arg(long)]
    workspace: Option<String>,
    #[arg(long)]
    project: Option<String>,
    #[arg(long, env = "AI_MEMORY_SERVER_URL", default_value = DEFAULT_SERVER_URL)]
    server_url: String,
    #[arg(long)]
    apply: bool,
    #[arg(long)]
    manifest_out: Option<PathBuf>,
    #[arg(long)]
    create_destination: bool,
    #[arg(long)]
    overwrite: bool,
    #[arg(long)]
    include_session_logs: bool,
    #[arg(long)]
    show_body: bool,
    #[arg(long)]
    pinned: bool,
}

#[derive(Parser, Debug, Clone)]
struct ConversationArgs {
    /// JSON file containing the generic conversation envelope.
    #[arg(long)]
    file: PathBuf,
    /// Explicit destination workspace. The project comes from the envelope.
    #[arg(long)]
    workspace: Option<String>,
    #[arg(long, env = "AI_MEMORY_SERVER_URL", default_value = DEFAULT_SERVER_URL)]
    server_url: String,
    /// Perform the replay. Without this flag the command only prints a plan.
    #[arg(long)]
    apply: bool,
    /// Durable replay manifest. Required with --apply.
    #[arg(long)]
    manifest_out: Option<PathBuf>,
    /// Permit the hook router to create a missing destination scope.
    #[arg(long)]
    create_destination: bool,
    /// Include sanitized message bodies in dry-run output.
    #[arg(long)]
    show_body: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConversationEnvelope {
    project: String,
    source: String,
    session_id: String,
    messages: Vec<ConversationMessage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConversationMessage {
    role: ConversationRole,
    content: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ConversationRole {
    System,
    User,
    Assistant,
}

impl ConversationRole {
    const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct PlannedHookEvent {
    index: usize,
    event: String,
    role: Option<ConversationRole>,
    ingest_key: String,
    url: String,
    body: serde_json::Value,
}

#[derive(Debug, Clone)]
struct ConversationPlan {
    source_path: String,
    source_sha256: String,
    source: String,
    external_session_id: String,
    stable_session_id: String,
    transcript_sha256: String,
    workspace: String,
    project: String,
    truncated_messages: usize,
    events: Vec<PlannedHookEvent>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ConversationManifestStatus {
    Planned,
    Imported,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
struct ConversationManifest {
    import_version: String,
    source_path: String,
    source_sha256: String,
    source: String,
    external_session_id: String,
    stable_session_id: String,
    transcript_sha256: String,
    workspace: String,
    project: String,
    planned_events: usize,
    truncated_messages: usize,
    accepted_events: usize,
    status: ConversationManifestStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct PlannedPage {
    source_path: String,
    source_sha256: String,
    destination_path: String,
    request: WritePageRequest,
}

#[derive(Debug, Serialize, Clone)]
struct ManifestEntry {
    import_version: String,
    source_path: String,
    source_sha256: String,
    destination_path: String,
    status: ManifestStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ManifestStatus {
    Planned,
    Written,
    Failed,
}

#[derive(Debug, Serialize, Clone)]
struct Manifest {
    import_version: String,
    entries: Vec<ManifestEntry>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
struct WritePageRequest {
    workspace: String,
    project: String,
    path: String,
    body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    tier: String,
    tags: Vec<String>,
    pinned: bool,
}

#[derive(Debug, Deserialize)]
struct WritePageResponse {
    page_id: String,
    path: String,
    checkpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PageListItem {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PageListBody {
    Bare(Vec<PageListItem>),
    Wrapped { pages: Vec<PageListItem> },
}

impl PageListBody {
    fn into_pages(self) -> Vec<PageListItem> {
        match self {
            Self::Bare(pages) | Self::Wrapped { pages } => pages,
        }
    }
}

#[derive(Debug, Serialize)]
struct HookBatchItem {
    url: String,
    body: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct HookBatchAck {
    accepted: usize,
    #[serde(default)]
    accepted_indices: Option<Vec<usize>>,
    #[serde(default)]
    failed_index: Option<usize>,
}

struct HookBatchDelivery {
    status: StatusCode,
    ack: HookBatchAck,
}

impl HookBatchAck {
    fn accepted_count(&self) -> usize {
        self.accepted_indices
            .as_ref()
            .map_or(self.accepted, Vec::len)
    }

    fn accepted_every_event(&self, expected: usize) -> bool {
        if self.failed_index.is_some() {
            return false;
        }
        match &self.accepted_indices {
            Some(indices) => indices.iter().copied().eq(0..expected),
            None => self.accepted == expected,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Commands::OmcWiki(args) => run_omc(args).await,
        Commands::ExternalConversation(args) => run_conversation(args).await,
        Commands::AgentmemoryVault(args) => agentmemory::vault::run(args).await,
        Commands::AgentmemoryExport(args) => agentmemory::export::run(args).await,
        Commands::ClaudeMem(args) => claude_mem::run(args).await,
        Commands::Document(args) => document::run(args).await,
    }
}

async fn run_conversation(args: ConversationArgs) -> Result<()> {
    if args.apply {
        if args
            .workspace
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            bail!("--apply requires explicit --workspace");
        }
        if args.manifest_out.is_none() {
            bail!("--apply requires --manifest-out <path>");
        }
    }

    let workspace = args
        .workspace
        .clone()
        .unwrap_or_else(|| "DRY-RUN-WORKSPACE".into());
    // Parse, bound, validate, and sanitize the entire source before creating an
    // HTTP client or writing a manifest. A malformed/hostile source therefore
    // cannot produce a partial import.
    let plan = plan_external_conversation(&args.file, &workspace, &args.server_url)?;
    let mut manifest = conversation_manifest(&plan);

    if !args.apply {
        if args.show_body {
            for event in &plan.events {
                println!(
                    "--- event {}: {} ({}) ---\n{}",
                    event.index,
                    event.event,
                    event.role.map_or("lifecycle", ConversationRole::as_str),
                    serde_json::to_string_pretty(&event.body)?
                );
            }
        }
        if let Some(path) = &args.manifest_out {
            write_conversation_manifest(path, &manifest)?;
        } else {
            println!(
                "dry-run: planned {} ordered hook events ({} truncated messages); no HTTP writes performed",
                plan.events.len(),
                plan.truncated_messages,
            );
            println!(
                "{}:{} -> {}/{} (stable session {})",
                plan.source,
                plan.external_session_id,
                plan.workspace,
                plan.project,
                plan.stable_session_id
            );
        }
        return Ok(());
    }

    let manifest_path = args.manifest_out.as_ref().unwrap();
    write_conversation_manifest(manifest_path, &manifest)?;
    let client = ImportClient::new(&args.server_url)?;
    if let Err(error) = client
        .preflight_project(&plan.workspace, &plan.project, args.create_destination)
        .await
    {
        manifest.status = ConversationManifestStatus::Failed;
        manifest.error = Some(error.to_string());
        write_conversation_manifest(manifest_path, &manifest)?;
        return Err(error);
    }

    let delivery = match client.post_hook_batch(&plan.events).await {
        Ok(delivery) => delivery,
        Err(error) => {
            manifest.status = ConversationManifestStatus::Failed;
            manifest.error = Some(error.to_string());
            write_conversation_manifest(manifest_path, &manifest)?;
            return Err(error);
        }
    };
    let ack = delivery.ack;
    manifest.accepted_events = ack.accepted_count();
    if !delivery.status.is_success() || !ack.accepted_every_event(plan.events.len()) {
        manifest.status = ConversationManifestStatus::Failed;
        manifest.error = Some(format!(
            "hook batch returned HTTP {} and accepted {} of {} events; failed_index={:?}; rerun the same source to resume safely",
            delivery.status,
            manifest.accepted_events,
            plan.events.len(),
            ack.failed_index
        ));
        write_conversation_manifest(manifest_path, &manifest)?;
        bail!(manifest.error.clone().unwrap());
    }
    manifest.status = ConversationManifestStatus::Imported;
    write_conversation_manifest(manifest_path, &manifest)?;
    println!(
        "import complete: replayed {} events into {}/{} as session {}",
        plan.events.len(),
        plan.workspace,
        plan.project,
        plan.stable_session_id
    );
    Ok(())
}

async fn run_omc(args: OmcArgs) -> Result<()> {
    if args.apply {
        if args
            .workspace
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
            || args.project.as_deref().is_none_or(|s| s.trim().is_empty())
        {
            bail!("--apply requires explicit --workspace and --project");
        }
        if args.manifest_out.is_none() {
            bail!("--apply requires --manifest-out <path>");
        }
    }
    let workspace = args
        .workspace
        .clone()
        .unwrap_or_else(|| "DRY-RUN-WORKSPACE".into());
    let project = args
        .project
        .clone()
        .unwrap_or_else(|| "DRY-RUN-PROJECT".into());
    let planned = plan_omc_wiki(
        &args.dir,
        &workspace,
        &project,
        args.include_session_logs,
        args.pinned,
    )?;
    let mut entries: Vec<_> = planned.iter().map(planned_entry).collect();

    if !args.apply {
        if args.show_body {
            for page in &planned {
                println!(
                    "--- {} -> {} ---\n{}",
                    page.source_path, page.destination_path, page.request.body
                );
            }
        }
        if let Some(path) = &args.manifest_out {
            write_manifest(path, &entries)?;
        } else {
            println!(
                "dry-run: planned {} writes; no HTTP writes performed",
                planned.len()
            );
            for page in &planned {
                println!("{} -> {}", page.source_path, page.destination_path);
            }
        }
        return Ok(());
    }

    let manifest_path = args.manifest_out.as_ref().unwrap();
    write_manifest(manifest_path, &entries)?;

    let client = ImportClient::new(&args.server_url)?;
    let destination_exists = client
        .preflight_project(&workspace, &project, args.create_destination)
        .await?;
    if !args.overwrite && destination_exists {
        let existing = client.existing_paths(&workspace, &project).await?;
        let conflicts: Vec<_> = planned
            .iter()
            .filter(|p| existing.contains_key(&p.destination_path))
            .collect();
        if !conflicts.is_empty() {
            bail!(
                "destination already has {} planned path(s); rerun with --overwrite to replace",
                conflicts.len()
            );
        }
    }

    for (idx, page) in planned.iter().enumerate() {
        if !args.overwrite
            && client
                .page_exists(&workspace, &project, &page.destination_path)
                .await?
        {
            entries[idx].status = ManifestStatus::Failed;
            entries[idx].error = Some("destination page appeared before write; aborting".into());
            write_manifest(manifest_path, &entries)?;
            bail!(
                "destination page appeared before write {}; completed {} writes",
                page.destination_path,
                idx
            );
        }
        match client.write_page(&page.request).await {
            Ok(resp) => {
                entries[idx].status = ManifestStatus::Written;
                entries[idx].page_id = Some(resp.page_id);
                entries[idx].path = Some(resp.path);
                entries[idx].checkpoint = resp.checkpoint;
                write_manifest(manifest_path, &entries)?;
            }
            Err(err) => {
                entries[idx].status = ManifestStatus::Failed;
                entries[idx].error = Some(err.to_string());
                write_manifest(manifest_path, &entries)?;
                bail!("live write failed after {} completed writes: {err}", idx);
            }
        }
    }
    println!(
        "import complete: wrote {} pages",
        entries
            .iter()
            .filter(|e| e.status == ManifestStatus::Written)
            .count()
    );
    Ok(())
}

fn plan_external_conversation(
    file: &Path,
    workspace: &str,
    server_url: &str,
) -> Result<ConversationPlan> {
    validate_label(workspace, "workspace", 128)?;
    ai_memory_client::reject_sensitive(&serde_json::json!([
        workspace,
        server_url,
        file.to_string_lossy().as_ref()
    ]))?;
    let metadata = fs::metadata(file)
        .with_context(|| format!("read conversation source {}", file.display()))?;
    if !metadata.is_file() {
        bail!("--file must be a regular file");
    }
    if metadata.len() > MAX_CONVERSATION_FILE_BYTES {
        bail!(
            "conversation file is {} bytes; maximum is {MAX_CONVERSATION_FILE_BYTES}",
            metadata.len()
        );
    }
    let bytes = fs::read(file).with_context(|| format!("read {}", file.display()))?;
    let source_sha256 = sha256_hex(&bytes);
    let mut envelope: ConversationEnvelope = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow!("parse generic conversation JSON: invalid schema"))?;

    ai_memory_client::reject_sensitive(&serde_json::json!({
        "project": envelope.project, "source": envelope.source, "session_id": envelope.session_id,
    }))?;
    envelope.project = validate_label(&envelope.project, "project", 128)?.to_owned();
    envelope.source = validate_source_name(&envelope.source)?.to_owned();
    if envelope.session_id.len() > 256 || envelope.session_id.chars().any(char::is_control) {
        bail!("session_id must be bounded and free of control characters");
    }
    validate_label(&envelope.session_id, "session_id", 256)?;
    if envelope.messages.is_empty() {
        bail!("conversation must contain at least one message");
    }
    if envelope.messages.len() > MAX_CONVERSATION_MESSAGES {
        bail!(
            "conversation has {} messages; maximum is {MAX_CONVERSATION_MESSAGES}",
            envelope.messages.len()
        );
    }

    let mut total_bytes = 0usize;
    let mut user_messages = 0usize;
    let mut truncated_messages = 0usize;
    for (index, message) in envelope.messages.iter_mut().enumerate() {
        total_bytes = total_bytes
            .checked_add(message.content.len())
            .ok_or_else(|| anyhow!("conversation content size overflow"))?;
        if total_bytes > MAX_CONVERSATION_TOTAL_BYTES {
            bail!(
                "conversation content is {total_bytes} bytes; maximum is {MAX_CONVERSATION_TOTAL_BYTES}"
            );
        }
        message.content = sanitize_external_text(&message.content);
        let per_event_cap = if message.role == ConversationRole::User {
            MAX_USER_MESSAGE_BYTES
        } else {
            MAX_EXTENSION_MESSAGE_BYTES
        };
        if message.content.len() > per_event_cap {
            message.content = truncate_utf8(&message.content, per_event_cap);
            truncated_messages += 1;
        }
        if message.content.trim().is_empty() {
            bail!("message {index} is empty after sanitization");
        }
        if message.role == ConversationRole::User {
            user_messages += 1;
        }
    }
    if user_messages == 0 {
        bail!("conversation must contain at least one user message");
    }

    let stable_session_id = stable_external_session_id(
        workspace,
        &envelope.project,
        &envelope.source,
        &envelope.session_id,
    );
    let transcript_sha256 = conversation_fingerprint(&envelope);
    let mut events = Vec::with_capacity(envelope.messages.len() + 2);
    let start_body = serde_json::json!({
        "session_id": stable_session_id,
        "model": truncate_utf8(&format!("external:{}", envelope.source), 160),
        "external_source": envelope.source,
        "external_session_id": envelope.session_id,
    });
    events.push(planned_hook_event(
        server_url,
        workspace,
        &envelope.project,
        &stable_session_id,
        0,
        "session-start",
        None,
        None,
        start_body,
    )?);

    for (message_index, message) in envelope.messages.iter().enumerate() {
        let event_index = message_index + 1;
        let (event, source_event, body) = match message.role {
            ConversationRole::User => (
                "user-prompt",
                None,
                serde_json::json!({
                    "session_id": stable_session_id,
                    "prompt": message.content,
                    "external_source": envelope.source,
                    "external_role": message.role.as_str(),
                }),
            ),
            ConversationRole::Assistant | ConversationRole::System => {
                let source_event = format!("{}-message", message.role.as_str());
                let title = first_line_title(&message.content, message.role);
                (
                    if message.role == ConversationRole::Assistant {
                        "external.assistant-message"
                    } else {
                        "external.system-message"
                    },
                    Some(source_event),
                    serde_json::json!({
                        "session_id": stable_session_id,
                        "title": title,
                        "message": message.content,
                        "external_source": envelope.source,
                        "external_role": message.role.as_str(),
                    }),
                )
            }
        };
        events.push(planned_hook_event(
            server_url,
            workspace,
            &envelope.project,
            &stable_session_id,
            event_index,
            event,
            Some(message.role),
            source_event.as_deref(),
            body,
        )?);
    }

    let end_index = envelope.messages.len() + 1;
    let end_body = serde_json::json!({
        "session_id": stable_session_id,
        "external_source": envelope.source,
        "external_session_id": envelope.session_id,
        "transcript_sha256": transcript_sha256,
    });
    events.push(planned_hook_event(
        server_url,
        workspace,
        &envelope.project,
        &stable_session_id,
        end_index,
        "session-end",
        None,
        None,
        end_body,
    )?);

    Ok(ConversationPlan {
        source_path: file.to_string_lossy().into_owned(),
        source_sha256,
        source: envelope.source,
        external_session_id: envelope.session_id,
        stable_session_id,
        transcript_sha256,
        workspace: workspace.to_owned(),
        project: envelope.project,
        truncated_messages,
        events,
    })
}

#[allow(clippy::too_many_arguments)]
fn planned_hook_event(
    server_url: &str,
    workspace: &str,
    project: &str,
    stable_session_id: &str,
    index: usize,
    event: &str,
    role: Option<ConversationRole>,
    source_event: Option<&str>,
    mut body: serde_json::Value,
) -> Result<PlannedHookEvent> {
    ai_memory_client::reject_sensitive(
        &serde_json::json!({"session_id": stable_session_id, "event": event, "source_event": source_event}),
    )?;
    if let Some(id) = body.get("session_id") {
        ai_memory_client::reject_sensitive(id)?;
    }
    ai_memory_client::sanitize_external_value(&mut body)?;
    ai_memory_client::check_body(&body)?;
    let body_bytes = serde_json::to_vec(&body)?;
    let ingest_key = event_ingest_key(stable_session_id, index, event, &body_bytes);
    let url = build_hook_url(
        server_url,
        event,
        workspace,
        project,
        stable_session_id,
        &ingest_key,
        source_event,
    )?;
    ai_memory_client::reject_sensitive(&serde_json::json!(url))?;
    Ok(PlannedHookEvent {
        index,
        event: event.to_owned(),
        role,
        ingest_key,
        url,
        body,
    })
}

fn build_hook_url(
    server_url: &str,
    event: &str,
    workspace: &str,
    project: &str,
    stable_session_id: &str,
    ingest_key: &str,
    source_event: Option<&str>,
) -> Result<String> {
    let mut url = Url::parse(server_url).context("invalid --server-url")?;
    let base = url.path().trim_end_matches('/');
    url.set_path(&format!("{base}/hook"));
    url.set_fragment(None);
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("event", event)
            .append_pair("agent", EXTERNAL_IMPORT_AGENT)
            .append_pair("workspace", workspace)
            .append_pair("project", project)
            .append_pair("project_src", "marker")
            .append_pair("session_id", stable_session_id)
            .append_pair("ingest_key", ingest_key);
        if let Some(source_event) = source_event {
            query
                .append_pair("extension", EXTERNAL_IMPORT_EXTENSION)
                .append_pair("source_event", source_event);
        }
    }
    Ok(url.into())
}

fn validate_label<'a>(value: &'a str, name: &str, max_bytes: usize) -> Result<&'a str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{name} must not be empty");
    }
    if trimmed.len() > max_bytes {
        bail!("{name} is {} bytes; maximum is {max_bytes}", trimmed.len());
    }
    if trimmed
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, '/' | '\\'))
    {
        bail!("{name} contains a control character or path separator");
    }
    Ok(trimmed)
}

fn validate_source_name(value: &str) -> Result<&str> {
    let source = validate_label(value, "source", 64)?;
    if !source
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        bail!("source must be an ASCII identifier using letters, digits, '.', '_', '-' or ':'");
    }
    Ok(source)
}

fn stable_external_session_id(
    workspace: &str,
    project: &str,
    source: &str,
    external_session_id: &str,
) -> String {
    let digest = sha256_parts(&[
        CONVERSATION_IMPORT_VERSION.as_bytes(),
        workspace.as_bytes(),
        project.as_bytes(),
        source.as_bytes(),
        external_session_id.as_bytes(),
    ]);
    format!("external-{}", &digest[..32])
}

fn conversation_fingerprint(envelope: &ConversationEnvelope) -> String {
    let mut hasher = Sha256::new();
    hash_part(&mut hasher, CONVERSATION_IMPORT_VERSION.as_bytes());
    hash_part(&mut hasher, envelope.project.as_bytes());
    hash_part(&mut hasher, envelope.source.as_bytes());
    hash_part(&mut hasher, envelope.session_id.as_bytes());
    for message in &envelope.messages {
        hash_part(&mut hasher, message.role.as_str().as_bytes());
        hash_part(&mut hasher, message.content.as_bytes());
    }
    hex_digest(hasher.finalize())
}

fn event_ingest_key(stable_session_id: &str, index: usize, event: &str, body: &[u8]) -> String {
    sha256_parts(&[
        CONVERSATION_IMPORT_VERSION.as_bytes(),
        stable_session_id.as_bytes(),
        index.to_string().as_bytes(),
        event.as_bytes(),
        body,
    ])
}

fn sha256_parts(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hash_part(&mut hasher, part);
    }
    hex_digest(hasher.finalize())
}

fn hash_part(hasher: &mut Sha256, part: &[u8]) {
    hasher.update(part.len().to_le_bytes());
    hasher.update(part);
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn first_line_title(content: &str, role: ConversationRole) -> String {
    let first = content.lines().find(|line| !line.trim().is_empty());
    first.map_or_else(
        || format!("Imported {} message", role.as_str()),
        |line| truncate_utf8(line.trim(), 160),
    )
}

fn truncate_utf8(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(start) = input[..end].rfind('[')
        && input[start..].starts_with("[REDACTED")
        && !input[start..end].contains(']')
    {
        end = start;
    }
    input[..end].to_owned()
}

fn conversation_manifest(plan: &ConversationPlan) -> ConversationManifest {
    ConversationManifest {
        import_version: CONVERSATION_IMPORT_VERSION.into(),
        source_path: plan.source_path.clone(),
        source_sha256: plan.source_sha256.clone(),
        source: plan.source.clone(),
        external_session_id: plan.external_session_id.clone(),
        stable_session_id: plan.stable_session_id.clone(),
        transcript_sha256: plan.transcript_sha256.clone(),
        workspace: plan.workspace.clone(),
        project: plan.project.clone(),
        planned_events: plan.events.len(),
        truncated_messages: plan.truncated_messages,
        accepted_events: 0,
        status: ConversationManifestStatus::Planned,
        error: None,
    }
}

fn write_conversation_manifest(path: &Path, manifest: &ConversationManifest) -> Result<()> {
    atomic_write(path, serde_json::to_string_pretty(manifest)?.as_bytes())
        .with_context(|| format!("write conversation manifest {}", path.display()))
}

fn planned_entry(page: &PlannedPage) -> ManifestEntry {
    ManifestEntry {
        import_version: IMPORT_VERSION.into(),
        source_path: page.source_path.clone(),
        source_sha256: page.source_sha256.clone(),
        destination_path: page.destination_path.clone(),
        status: ManifestStatus::Planned,
        page_id: None,
        path: None,
        checkpoint: None,
        error: None,
    }
}

fn write_manifest(path: &Path, entries: &[ManifestEntry]) -> Result<()> {
    let manifest = Manifest {
        import_version: IMPORT_VERSION.into(),
        entries: entries.to_vec(),
    };
    atomic_write(path, serde_json::to_string_pretty(&manifest)?.as_bytes())
        .with_context(|| format!("write manifest {}", path.display()))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent
        && !parent.exists()
    {
        bail!(
            "manifest parent directory does not exist: {}",
            parent.display()
        );
    }
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("manifest path must include a file name"))?;
    let directory = parent.unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{file_name}."))
        .suffix(".tmp")
        .tempfile_in(directory)?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    fs::File::open(directory)?.sync_all()?;
    Ok(())
}

fn plan_omc_wiki(
    dir: &Path,
    workspace: &str,
    project: &str,
    include_session_logs: bool,
    pinned: bool,
) -> Result<Vec<PlannedPage>> {
    let root =
        fs::canonicalize(dir).with_context(|| format!("read source dir {}", dir.display()))?;
    if !root.is_dir() {
        bail!("--dir must be a directory");
    }
    let mut planned = Vec::new();
    let mut destinations: HashMap<String, String> = HashMap::new();
    for entry in WalkDir::new(&root)
        .min_depth(1)
        .max_depth(1)
        .sort_by_file_name()
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let rel = path
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace('\\', "/");
        validate_source_rel(&rel)?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("invalid filename"))?;
        if name == "index.md" || (!include_session_logs && name.starts_with("session-log-")) {
            continue;
        }
        let content =
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let source_sha256 = sha256_hex(content.as_bytes());
        let parsed = parse_markdown(&content)?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("invalid filename"))?;
        let dest = format!("omc/{}.md", slugify(stem));
        validate_destination_path(&dest)?;
        if let Some(other) = destinations.insert(dest.clone(), rel.clone()) {
            bail!("duplicate destination path collision: {other} and {rel} both map to {dest}");
        }
        planned.push(PlannedPage {
            source_path: rel,
            source_sha256,
            destination_path: dest.clone(),
            request: WritePageRequest {
                workspace: workspace.to_owned(),
                project: project.to_owned(),
                path: dest,
                body: parsed.body,
                title: parsed.title,
                kind: parsed.kind,
                tier: normalize_tier(parsed.tier.as_deref())?,
                tags: parsed.tags,
                pinned: parsed.pinned || pinned,
            },
        });
    }
    Ok(planned)
}

fn normalize_tier(tier: Option<&str>) -> Result<String> {
    let tier = tier.unwrap_or("semantic").trim();
    match tier {
        "working" | "episodic" | "semantic" | "procedural" => Ok(tier.to_owned()),
        other => bail!("unsupported tier '{other}'"),
    }
}

#[derive(Default)]
struct ParsedMarkdown {
    body: String,
    title: Option<String>,
    kind: Option<String>,
    tier: Option<String>,
    tags: Vec<String>,
    pinned: bool,
}

/// Split an OMC page into frontmatter fields and body.
fn parse_markdown(input: &str) -> Result<ParsedMarkdown> {
    let mut out = ParsedMarkdown::default();
    let (frontmatter, body) = split_frontmatter(input)?;
    if let Some(map) = frontmatter.as_ref() {
        out.title = yaml_string(map, "title");
        out.kind = yaml_string(map, "kind");
        out.tier = yaml_string(map, "tier");
        out.pinned = yaml_bool(map, "pinned").unwrap_or(false);
        out.tags = yaml_tags(map);
    }
    if out.title.is_none() {
        out.title = first_h1(&body);
    }
    out.body = body;
    Ok(out)
}

/// Split a Markdown page into its YAML frontmatter mapping (when the block
/// is a mapping) and its body. The fence is found the way
/// `ai_memory_wiki::markdown::parse` finds it: past a leading UTF-8 BOM, and
/// with either LF or CRLF fence lines, since a wiki checked out on Windows
/// with `core.autocrlf=true` has CRLF throughout. Missing it dropped kind,
/// tier, tags and pin, and imported the YAML block as body text.
fn split_frontmatter(input: &str) -> Result<(Option<YamlMap>, String)> {
    let input = input.strip_prefix('\u{FEFF}').unwrap_or(input);
    let (rest, newline) = if let Some(rest) = input.strip_prefix("---\r\n") {
        (Some(rest), "\r\n")
    } else {
        (input.strip_prefix("---\n"), "\n")
    };
    let close = format!("\n---{newline}");
    let Some(rest) = rest else {
        return Ok((None, input.to_owned()));
    };
    // The closing fence may also end the file with no line ending after it.
    let split = if let Some(body) = rest.strip_prefix(&close[1..]) {
        Some(("", body))
    } else if rest == "---" {
        Some(("", ""))
    } else {
        rest.find(&close)
            .map(|end| (&rest[..end], &rest[end + close.len()..]))
            .or_else(|| rest.strip_suffix("\n---").map(|yaml| (yaml, "")))
    };
    let Some((yaml, body)) = split else {
        return Ok((None, input.to_owned()));
    };
    let yaml = yaml.trim_end_matches('\r');
    let value: serde_json::Value =
        serde_saphyr::from_str_with_options(yaml, frontmatter_parse_options())
            .context("parse YAML frontmatter")?;
    let map = match value {
        serde_json::Value::Object(map) => Some(map),
        _ => None,
    };
    Ok((map, body.to_owned()))
}

/// Read frontmatter the way `serde_yaml` 0.9 did, matching the server's
/// parser: YAML 1.2 booleans (`pinned: yes` stays a string, so not pinned),
/// `<<` as an ordinary key, and `.inf`/`.nan` kept as text instead of
/// failing the page.
fn frontmatter_parse_options() -> serde_saphyr::Options {
    serde_saphyr::options! {
        strict_booleans: true,
        merge_keys: serde_saphyr::MergeKeyPolicy::AsOrdinary,
        ignore_binary_tag_for_string: true,
        reject_non_finite_typeless_float: false,
        with_snippet: false,
        budget: serde_saphyr::budget! { max_depth: 128 },
    }
}

type YamlMap = serde_json::Map<String, serde_json::Value>;

fn yaml_string(map: &YamlMap, key: &str) -> Option<String> {
    map.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .filter(|s| !s.trim().is_empty())
}
fn yaml_bool(map: &YamlMap, key: &str) -> Option<bool> {
    map.get(key).and_then(|v| v.as_bool())
}
fn yaml_tags(map: &YamlMap) -> Vec<String> {
    match map.get("tags") {
        Some(serde_json::Value::Array(seq)) => seq
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        Some(v) => v.as_str().map(|s| vec![s.to_owned()]).unwrap_or_default(),
        None => Vec::new(),
    }
}
fn first_h1(body: &str) -> Option<String> {
    first_h1_line(body)
        .map(str::trim)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

/// The text after `# ` on the first H1 line outside fenced code. Inside a
/// fence a `# ` line is a shell or Python comment; fences follow CommonMark
/// as the server's `derive_title` reads them (a run of 3+ backticks or
/// tildes indented at most 3 spaces, closed by a bare run of the same glyph
/// at least as long).
fn first_h1_line(body: &str) -> Option<&str> {
    let mut fence: Option<(char, usize)> = None;
    for line in body.lines() {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let marker = &line[indent..];
        if let Some(glyph) = marker.chars().next().filter(|c| matches!(c, '`' | '~'))
            && indent <= 3
        {
            let run = marker.chars().take_while(|&c| c == glyph).count();
            let info = &marker[run..];
            if run >= 3 {
                match fence {
                    // A backtick info string cannot hold a backtick: this is
                    // an inline code span, not a fence.
                    None if glyph == '`' && info.contains('`') => {}
                    None => {
                        fence = Some((glyph, run));
                        continue;
                    }
                    Some((open, len)) if open == glyph && run >= len && info.trim().is_empty() => {
                        fence = None;
                        continue;
                    }
                    Some(_) => continue,
                }
            }
        }
        if fence.is_none()
            && let Some(rest) = line.strip_prefix("# ")
        {
            return Some(rest);
        }
    }
    None
}

fn validate_source_rel(rel: &str) -> Result<()> {
    let p = Path::new(rel);
    if p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("unsafe source relative path: {rel}");
    }
    Ok(())
}

fn validate_destination_path(path: &str) -> Result<()> {
    let p = Path::new(path);
    if p.is_absolute()
        || path.starts_with('/')
        || path.contains('\\')
        || p.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("unsafe destination path: {path}");
    }
    let first = path.split('/').next().unwrap_or_default();
    if matches!(
        first,
        "_rules"
            | "_internal"
            | ".git"
            | "sessions"
            | "session-logs"
            | "procedures"
            | "decisions"
            | "gotchas"
    ) {
        bail!("reserved destination prefix: {first}");
    }
    if !path.ends_with(".md") {
        bail!("destination path must end in .md");
    }
    Ok(())
}

fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in s.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "page".into()
    } else {
        trimmed
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

struct ImportClient {
    client: reqwest::Client,
    origin: String,
    base_path: String,
    token: Option<String>,
    mcp_session: Mutex<Option<Option<String>>>,
    mcp_request_id: AtomicU64,
}

impl ImportClient {
    fn new(server_url: &str) -> Result<Self> {
        let url = Url::parse(server_url).context("invalid --server-url")?;
        let origin = format!(
            "{}://{}",
            url.scheme(),
            url.host_str()
                .ok_or_else(|| anyhow!("server URL needs host"))?
        );
        let origin = if let Some(port) = url.port() {
            format!("{origin}:{port}")
        } else {
            origin
        };
        let base_path = url.path().trim_end_matches('/').to_owned();
        let base_path = if base_path == "/" {
            String::new()
        } else {
            base_path
        };
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            origin,
            base_path,
            token: std::env::var("AI_MEMORY_AUTH_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
            mcp_session: Mutex::new(None),
            mcp_request_id: AtomicU64::new(0),
        })
    }
    fn url(&self, path: &str) -> String {
        format!("{}{}{}", self.origin, self.base_path, path)
    }
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let req = self.client.request(method, self.url(path));
        if let Some(token) = &self.token {
            req.bearer_auth(token)
        } else {
            req
        }
    }
    async fn preflight_project(
        &self,
        workspace: &str,
        project: &str,
        create: bool,
    ) -> Result<bool> {
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!(
                    "/api/v1/workspaces/{}/projects/{}/pages",
                    enc(workspace),
                    enc(project)
                ),
            )
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(true)
        } else if resp.status() == StatusCode::NOT_FOUND && create {
            Ok(false)
        } else {
            bail!(
                "destination workspace/project must already exist (or pass --create-destination): HTTP {}",
                resp.status()
            )
        }
    }
    async fn existing_paths(&self, workspace: &str, project: &str) -> Result<HashMap<String, ()>> {
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!(
                    "/api/v1/workspaces/{}/projects/{}/pages",
                    enc(workspace),
                    enc(project)
                ),
            )
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("list destination pages failed: HTTP {}", resp.status());
        }
        let pages: PageListBody = resp.json().await?;
        Ok(pages
            .into_pages()
            .into_iter()
            .map(|page| (page.path, ()))
            .collect())
    }
    async fn page_exists(&self, workspace: &str, project: &str, path: &str) -> Result<bool> {
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!(
                    "/api/v1/workspaces/{}/projects/{}/pages/{}",
                    enc(workspace),
                    enc(project),
                    enc_path(path)
                ),
            )
            .send()
            .await?;
        match resp.status() {
            StatusCode::OK => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            s => bail!("page pre-write check failed: HTTP {s}"),
        }
    }
    async fn write_page(&self, req: &WritePageRequest) -> Result<WritePageResponse> {
        let resp = self
            .request(reqwest::Method::POST, "/admin/write-page")
            .json(req)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("write-page failed: HTTP {}", resp.status());
        }
        Ok(resp.json().await?)
    }

    async fn post_hook_batch(&self, events: &[PlannedHookEvent]) -> Result<HookBatchDelivery> {
        for event in events {
            ai_memory_client::check_body(&event.body)?;
            ai_memory_client::reject_sensitive(&serde_json::json!(event.url))?;
        }
        let items: Vec<_> = events
            .iter()
            .map(|event| HookBatchItem {
                url: event.url.clone(),
                body: event.body.clone(),
            })
            .collect();
        let resp = self
            .request(reqwest::Method::POST, "/hook/batch")
            .json(&items)
            .send()
            .await?;
        let status = resp.status();
        let ack = resp
            .json()
            .await
            .with_context(|| format!("hook batch returned HTTP {status} with an invalid ack"))?;
        Ok(HookBatchDelivery { status, ack })
    }
}

fn enc(s: &str) -> String {
    // Form encoding writes a space as `+`, which a URL path keeps literally;
    // a literal `+` is already `%2B`, so every `+` left is a space.
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}
fn enc_path(s: &str) -> String {
    s.split('/').map(enc).collect::<Vec<_>>().join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Path segments are percent-encoded: a space is `%20`. Form encoding's
    /// `+` reaches the server as a literal `+`, so a project named "My App"
    /// was looked up as "My+App".
    #[test]
    fn path_segments_encode_a_space_as_percent_20() {
        assert_eq!(enc("My App"), "My%20App");
        assert_eq!(enc("a+b"), "a%2Bb");
        assert_eq!(enc_path("omc/my notes.md"), "omc/my%20notes.md");
    }
    use crate::test_http::{read_http_request, respond_json};
    use std::net::TcpListener;
    use std::thread;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    fn write_conversation(dir: &Path, body: serde_json::Value) -> PathBuf {
        let path = dir.join("conversation.json");
        fs::write(&path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
        path
    }

    fn sample_conversation() -> serde_json::Value {
        serde_json::json!({
            "project": "memory-lab",
            "source": "chatgpt",
            "session_id": "source-conversation-42",
            "messages": [
                {"role": "system", "content": "Answer concisely."},
                {"role": "user", "content": "Why is the queue bounded?"},
                {"role": "assistant", "content": "To make backpressure explicit."}
            ]
        })
    }

    #[test]
    fn atomic_manifest_write_replaces_existing_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("manifest.json");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn atomic_manifest_write_does_not_follow_legacy_predictable_temp_symlink() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("manifest.json");
        let victim = temp.path().join("victim");
        fs::write(&victim, "unchanged").unwrap();
        std::os::unix::fs::symlink(&victim, temp.path().join(".manifest.json.tmp")).unwrap();

        atomic_write(&path, b"manifest").unwrap();

        assert_eq!(fs::read_to_string(victim).unwrap(), "unchanged");
        assert_eq!(fs::read(path).unwrap(), b"manifest");
    }

    #[test]
    fn parses_omc_frontmatter() {
        let parsed = parse_markdown("---\ntitle: T\nkind: rule\ntier: procedural\ntags: [a, b]\npinned: true\nextra: ignored\n---\n# Body\ntext").unwrap();
        assert_eq!(parsed.title.as_deref(), Some("T"));
        assert_eq!(parsed.kind.as_deref(), Some("rule"));
        assert_eq!(parsed.tier.as_deref(), Some("procedural"));
        assert_eq!(parsed.tags, vec!["a", "b"]);
        assert!(parsed.pinned);
        assert_eq!(parsed.body, "# Body\ntext");
    }

    /// `serde_yaml` 0.9 read YAML 1.1 words as text, so `pinned: yes` never
    /// pinned a page and a bare `tags:` word stayed a one-item list.
    #[test]
    fn omc_frontmatter_keeps_yaml_1_1_words_as_text() {
        let parsed = parse_markdown("---\ntitle: yes\npinned: yes\ntags: on\n---\nbody").unwrap();
        assert_eq!(parsed.title.as_deref(), Some("yes"));
        assert!(!parsed.pinned);
        assert_eq!(parsed.tags, vec!["on"]);

        assert!(parse_markdown("---\ntitle: a\ntitle: b\n---\nbody").is_err());
    }

    /// A `# ` line inside fenced code is a shell or Python comment, not the
    /// page's H1; the server's `derive_title` skips fences the same way.
    #[test]
    fn title_skips_comment_lines_inside_code_fences() {
        let parsed = parse_markdown("```bash\n# install deps\nnpm i\n```\n# Real Title\n").unwrap();
        assert_eq!(parsed.title.as_deref(), Some("Real Title"));
        let tilde = parse_markdown("~~~~\n# not it\n~~~\n# still code\n~~~~\n# Title\n").unwrap();
        assert_eq!(tilde.title.as_deref(), Some("Title"));
        let none = parse_markdown("```sh\n# just a comment\n```\nbody\n").unwrap();
        assert_eq!(none.title, None);
        let inline = parse_markdown("```code``` text\n# After\n").unwrap();
        assert_eq!(inline.title.as_deref(), Some("After"));
    }

    /// A frontmatter-only page whose closing fence ends the file with no
    /// final newline, which the server's parser has accepted since #1402.
    #[test]
    fn parses_omc_frontmatter_closed_at_end_of_file() {
        let parsed = parse_markdown("---\ntitle: T\ntier: procedural\npinned: true\n---").unwrap();
        assert_eq!(parsed.title.as_deref(), Some("T"));
        assert_eq!(parsed.tier.as_deref(), Some("procedural"));
        assert!(parsed.pinned);
        assert_eq!(parsed.body, "");

        let crlf = parse_markdown("\u{feff}---\r\ntitle: T\r\n---").unwrap();
        assert_eq!(crlf.title.as_deref(), Some("T"));
        assert_eq!(crlf.body, "");

        let not_a_fence = parse_markdown("---\ntitle: T\n----").unwrap();
        assert_eq!(not_a_fence.title, None);

        let empty = parse_markdown("---\n---").unwrap();
        assert_eq!(empty.body, "");
        let empty_with_body = parse_markdown("---\n---\n# Body\n").unwrap();
        assert_eq!(empty_with_body.title.as_deref(), Some("Body"));
        assert_eq!(empty_with_body.body, "# Body\n");
    }

    /// A wiki checked out on Windows with `core.autocrlf=true`, or saved
    /// there by an editor, has CRLF line endings; one saved as "UTF-8 with
    /// BOM" opens with U+FEFF. Either used to miss the fence, so kind, tier,
    /// tags and pin were dropped and the YAML block was imported as body.
    #[test]
    fn parses_omc_frontmatter_with_crlf_or_a_bom() {
        let crlf = "---\r\ntitle: T\r\nkind: rule\r\ntier: procedural\r\ntags: [a, b]\r\npinned: true\r\n---\r\n# Body\r\ntext";
        let parsed = parse_markdown(crlf).unwrap();
        assert_eq!(parsed.title.as_deref(), Some("T"));
        assert_eq!(parsed.kind.as_deref(), Some("rule"));
        assert_eq!(parsed.tier.as_deref(), Some("procedural"));
        assert_eq!(parsed.tags, vec!["a", "b"]);
        assert!(parsed.pinned);
        assert_eq!(
            parsed.body, "# Body\r\ntext",
            "the body keeps its line endings"
        );

        let bom = parse_markdown("\u{feff}---\ntier: procedural\n---\n# Body\ntext").unwrap();
        assert_eq!(bom.tier.as_deref(), Some("procedural"));
        assert_eq!(bom.title.as_deref(), Some("Body"));
        assert_eq!(bom.body, "# Body\ntext");
    }

    #[test]
    fn planning_rejects_unknown_tier_before_live_write() {
        let td = tempdir().unwrap();
        write(td.path(), "note.md", "---\ntier: legendary\n---\n# Note");
        assert!(
            plan_omc_wiki(td.path(), "w", "p", false, false)
                .unwrap_err()
                .to_string()
                .contains("unsupported tier")
        );
    }

    #[test]
    fn skips_index_and_session_logs_by_default() {
        let td = tempdir().unwrap();
        write(td.path(), "index.md", "# Index");
        write(td.path(), "session-log-1.md", "# Log");
        write(td.path(), "note.md", "# Note");
        let pages = plan_omc_wiki(td.path(), "w", "p", false, false).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].destination_path, "omc/note.md");
    }

    #[test]
    fn path_traversal_rejected() {
        assert!(validate_source_rel("../x.md").is_err());
        assert!(validate_destination_path("omc/../x.md").is_err());
        assert!(validate_destination_path("/omc/x.md").is_err());
    }

    #[test]
    fn detects_duplicate_slug_collisions() {
        let td = tempdir().unwrap();
        write(td.path(), "A B.md", "# A");
        write(td.path(), "a-b.md", "# B");
        assert!(
            plan_omc_wiki(td.path(), "w", "p", false, false)
                .unwrap_err()
                .to_string()
                .contains("collision")
        );
    }

    #[test]
    fn dry_run_planning_has_no_http_client() {
        let td = tempdir().unwrap();
        write(td.path(), "note.md", "# Note");
        let pages = plan_omc_wiki(td.path(), "w", "p", false, false).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].request.workspace, "w");
    }

    #[test]
    fn write_request_json_shape() {
        let req = WritePageRequest {
            workspace: "w".into(),
            project: "p".into(),
            path: "omc/n.md".into(),
            body: "# N".into(),
            title: Some("N".into()),
            kind: Some("fact".into()),
            tier: "semantic".into(),
            tags: vec!["omc".into()],
            pinned: true,
        };
        let v = serde_json::to_value(req).unwrap();
        assert_eq!(v["workspace"], "w");
        assert_eq!(v["project"], "p");
        assert_eq!(v["path"], "omc/n.md");
        assert_eq!(v["body"], "# N");
        assert_eq!(v["pinned"], true);
    }

    #[test]
    fn parses_current_bare_api_page_list_shape() {
        let body = r#"[{"path":"omc/a.md"},{"path":"notes/b.md"}]"#;
        let parsed: PageListBody = serde_json::from_str(body).unwrap();
        let paths: Vec<_> = parsed.into_pages().into_iter().map(|p| p.path).collect();
        assert_eq!(paths, vec!["omc/a.md", "notes/b.md"]);
    }

    #[test]
    fn parses_legacy_wrapped_api_page_list_shape() {
        let body = r#"{"pages":[{"path":"omc/a.md"},{"path":"notes/b.md"}]}"#;
        let parsed: PageListBody = serde_json::from_str(body).unwrap();
        let paths: Vec<_> = parsed.into_pages().into_iter().map(|p| p.path).collect();
        assert_eq!(paths, vec!["omc/a.md", "notes/b.md"]);
    }

    #[test]
    fn auth_header_uses_bearer_scheme() {
        let rb = reqwest::Client::new()
            .get("http://127.0.0.1/")
            .bearer_auth("secret-token");
        let req = rb.build().unwrap();
        assert_eq!(
            req.headers().get(reqwest::header::AUTHORIZATION).unwrap(),
            "Bearer secret-token"
        );
        let dbg = format!("{:?}", req.headers());
        assert!(!dbg.contains("AI_MEMORY_AUTH_TOKEN"));
    }

    #[test]
    fn overwrite_requires_explicit_flag() {
        let args = OmcArgs {
            dir: PathBuf::from("."),
            workspace: Some("w".into()),
            project: Some("p".into()),
            server_url: DEFAULT_SERVER_URL.into(),
            apply: true,
            manifest_out: Some(PathBuf::from("m.json")),
            create_destination: false,
            overwrite: false,
            include_session_logs: false,
            show_body: false,
            pinned: false,
        };
        assert!(!args.overwrite);
    }

    #[test]
    fn external_plan_is_ordered_bounded_and_does_not_impersonate_a_live_agent() {
        let td = tempdir().unwrap();
        let file = write_conversation(td.path(), sample_conversation());
        let plan = plan_external_conversation(&file, "default", DEFAULT_SERVER_URL).unwrap();

        assert_eq!(plan.events.len(), 5);
        assert_eq!(plan.events[0].event, "session-start");
        assert_eq!(plan.events[2].event, "user-prompt");
        assert_eq!(plan.events[3].event, "external.assistant-message");
        assert_eq!(plan.events[4].event, "session-end");
        for (index, event) in plan.events.iter().enumerate() {
            assert_eq!(event.index, index);
            assert_eq!(event.ingest_key.len(), 64);
            let url = Url::parse(&event.url).unwrap();
            let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(
                query.get("agent").map(String::as_str),
                Some("external-import")
            );
            assert_ne!(query.get("agent").map(String::as_str), Some("codex"));
            assert_ne!(query.get("agent").map(String::as_str), Some("claude-code"));
        }
        let assistant_query: HashMap<_, _> = Url::parse(&plan.events[3].url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        assert_eq!(
            assistant_query.get("extension").map(String::as_str),
            Some(EXTERNAL_IMPORT_EXTENSION)
        );
        assert_eq!(
            assistant_query.get("source_event").map(String::as_str),
            Some("assistant-message")
        );
    }

    #[test]
    fn stable_ids_make_replays_idempotent_and_new_generations_reend() {
        let td = tempdir().unwrap();
        let file = write_conversation(td.path(), sample_conversation());
        let first = plan_external_conversation(&file, "default", DEFAULT_SERVER_URL).unwrap();
        let replay = plan_external_conversation(&file, "default", DEFAULT_SERVER_URL).unwrap();
        assert_eq!(first.stable_session_id, replay.stable_session_id);
        assert_eq!(first.transcript_sha256, replay.transcript_sha256);
        assert_eq!(
            first
                .events
                .iter()
                .map(|event| &event.ingest_key)
                .collect::<Vec<_>>(),
            replay
                .events
                .iter()
                .map(|event| &event.ingest_key)
                .collect::<Vec<_>>()
        );

        let mut extended = sample_conversation();
        extended["messages"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"role": "user", "content": "What changed?"}));
        write_conversation(td.path(), extended);
        let next = plan_external_conversation(&file, "default", DEFAULT_SERVER_URL).unwrap();
        assert_eq!(first.stable_session_id, next.stable_session_id);
        assert_ne!(first.transcript_sha256, next.transcript_sha256);
        assert_eq!(first.events[0].ingest_key, next.events[0].ingest_key);
        assert_ne!(
            first.events.last().unwrap().ingest_key,
            next.events.last().unwrap().ingest_key,
            "a changed transcript needs a fresh SessionEnd generation"
        );
    }

    #[test]
    fn external_text_is_sanitized_before_any_hook_body_is_built() {
        let td = tempdir().unwrap();
        let secret = "github_pat_1234567890abcdefghijklmnop";
        let file = write_conversation(
            td.path(),
            serde_json::json!({
                "project": "p",
                "source": "chatgpt",
                "session_id": "s",
                "messages": [
                    {"role": "user", "content": format!("token={secret}\nBearer abcdefghijklmnop")}
                ]
            }),
        );
        let plan = plan_external_conversation(&file, "w", DEFAULT_SERVER_URL).unwrap();
        let serialized = serde_json::to_string(&plan.events).unwrap();
        assert!(!serialized.contains(secret));
        assert!(!serialized.contains("abcdefghijklmnop"));
        assert!(serialized.contains("REDACTED"));
    }

    #[tokio::test]
    async fn malformed_or_hostile_source_fails_before_manifest_or_http() {
        let td = tempdir().unwrap();
        let file = write_conversation(
            td.path(),
            serde_json::json!({
                "project": "p",
                "source": "chatgpt",
                "session_id": "s",
                "messages": [{"role": "tool", "content": "hostile"}],
                "unexpected": true
            }),
        );
        let manifest = td.path().join("manifest.json");
        let error = run_conversation(ConversationArgs {
            file,
            workspace: Some("w".into()),
            // If planning accidentally reaches HTTP setup, this URL is invalid.
            server_url: "not a url".into(),
            apply: true,
            manifest_out: Some(manifest.clone()),
            create_destination: false,
            show_body: false,
        })
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("parse generic conversation JSON")
        );
        assert!(
            !manifest.exists(),
            "invalid input must have zero side effects"
        );
    }

    #[tokio::test]
    async fn live_import_round_trips_preflight_and_one_ordered_hook_batch() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut preflight, _) = listener.accept().unwrap();
            let (headers, body) = read_http_request(&mut preflight);
            assert!(
                headers.starts_with("GET /api/v1/workspaces/default/projects/memory-lab/pages ")
            );
            assert!(body.is_empty());
            respond_json(&mut preflight, "200 OK", "[]");

            let (mut hook, _) = listener.accept().unwrap();
            let (headers, body) = read_http_request(&mut hook);
            assert!(headers.starts_with("POST /hook/batch "));
            let batch: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let items = batch.as_array().unwrap();
            assert_eq!(items.len(), 5);
            assert!(
                items[0]["url"]
                    .as_str()
                    .unwrap()
                    .contains("event=session-start")
            );
            assert!(
                items[2]["url"]
                    .as_str()
                    .unwrap()
                    .contains("event=user-prompt")
            );
            assert!(
                items[4]["url"]
                    .as_str()
                    .unwrap()
                    .contains("event=session-end")
            );
            assert!(items.iter().all(|item| {
                item["url"]
                    .as_str()
                    .unwrap()
                    .contains("agent=external-import")
            }));
            respond_json(&mut hook, "200 OK", r#"{"accepted":5}"#);
        });

        let td = tempdir().unwrap();
        let file = write_conversation(td.path(), sample_conversation());
        let manifest = td.path().join("manifest.json");
        run_conversation(ConversationArgs {
            file,
            workspace: Some("default".into()),
            server_url: format!("http://{address}"),
            apply: true,
            manifest_out: Some(manifest.clone()),
            create_destination: false,
            show_body: false,
        })
        .await
        .unwrap();
        server.join().unwrap();

        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(manifest).unwrap()).unwrap();
        assert_eq!(persisted["status"], "imported");
        assert_eq!(persisted["accepted_events"], 5);
        assert_eq!(persisted["planned_events"], 5);
    }

    #[test]
    fn conversation_caps_reject_before_planning_events() {
        let td = tempdir().unwrap();
        let messages: Vec<_> = (0..=MAX_CONVERSATION_MESSAGES)
            .map(|index| serde_json::json!({"role": "user", "content": format!("m{index}")}))
            .collect();
        let file = write_conversation(
            td.path(),
            serde_json::json!({
                "project": "p",
                "source": "chatgpt",
                "session_id": "s",
                "messages": messages
            }),
        );
        let error = plan_external_conversation(&file, "w", DEFAULT_SERVER_URL).unwrap_err();
        assert!(error.to_string().contains("maximum"));
    }

    #[test]
    fn oversized_messages_are_utf8_safely_truncated_to_hook_event_caps() {
        let td = tempdir().unwrap();
        let file = write_conversation(
            td.path(),
            serde_json::json!({
                "project": "p",
                "source": "chatgpt",
                "session_id": "s",
                "messages": [
                    {"role": "user", "content": "界".repeat(MAX_USER_MESSAGE_BYTES)},
                    {"role": "assistant", "content": "界".repeat(MAX_EXTENSION_MESSAGE_BYTES)}
                ]
            }),
        );
        let plan = plan_external_conversation(&file, "w", DEFAULT_SERVER_URL).unwrap();
        assert_eq!(plan.truncated_messages, 2);
        assert!(plan.events[1].body["prompt"].as_str().unwrap().len() <= MAX_USER_MESSAGE_BYTES);
        assert!(
            plan.events[2].body["message"].as_str().unwrap().len() <= MAX_EXTENSION_MESSAGE_BYTES
        );
    }

    #[test]
    fn batch_ack_requires_every_index_and_no_failure() {
        assert!(
            HookBatchAck {
                accepted: 3,
                accepted_indices: None,
                failed_index: None,
            }
            .accepted_every_event(3)
        );
        assert!(
            HookBatchAck {
                accepted: 0,
                accepted_indices: Some(vec![0, 1, 2]),
                failed_index: None,
            }
            .accepted_every_event(3)
        );
        assert!(
            !HookBatchAck {
                accepted: 1,
                accepted_indices: Some(vec![0, 2]),
                failed_index: Some(1),
            }
            .accepted_every_event(3)
        );
    }
    #[tokio::test]
    async fn shared_key_sanitation_precedes_plan_manifest_transport_and_exact_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut raw = serde_json::json!({"session_id":"native", "cwd":"/work/app", "output":"\u{1b}[32mresult\u{1b}[0m\u{202e} token=getToken() Bearer abcdefghijklmnop", "nested":[{"accessToken":"fixture-value", "refreshToken":"fixture-value", "clientSecret":"fixture-value", "privateKey":"fixture-value", "x-api-key":"fixture-value", "OPENAI_API_KEY":"fixture-value", "cookie":"fixture-value", "credentials":"fixture-value"}], "metrics":{"max_tokens":12,"input_tokens":34,"token_count":0,"token_usage":1.25}});
        raw["metrics"] = serde_json::json!({"max_tokens":12,"input_tokens":34,"token_count":0,"token_usage":1.25,"output_tokens":9007199254740993_u64,"total_tokens":89,"totalTokens":89,"cache_read_input_tokens":2,"cache_creation_input_tokens":3,"prompt_tokens":5,"completion_tokens":7,"reasoning_tokens":11});
        raw["key_forms"] = serde_json::json!([{"auth":"fixture-value","jwt":"fixture-value","pwd":"fixture-value","bearer":"fixture-value","session_key":"fixture-value","signature":"fixture-value"}]);
        raw["plural_forms"] = serde_json::json!([{
            "api_keys":"fixture-value", "access_keys":"fixture-value", "private_keys":"fixture-value", "session_keys":"fixture-value",
            "apiKeys":["fixture-value"], "accessKeys":["fixture-value"], "privateKeys":["fixture-value"], "sessionKeys":["fixture-value"]
        }]);
        raw["controls"] = serde_json::json!({"author":"fixture-value","authority":"fixture-value","key":"fixture-value","keys":["fixture-value"]});
        raw["numeric_cases"] = serde_json::json!({"token":123456,"access_tokens":123456,"private_token_count":123456,"auth_token_usage":123456,"token_usage":{"input_tokens":12}});
        let planned = || {
            planned_hook_event(
                &base,
                "w",
                "p",
                "native",
                0,
                "post-tool-use",
                None,
                None,
                raw.clone(),
            )
            .unwrap()
        };
        let event = planned();
        assert_eq!(event.body["session_id"], raw["session_id"]);
        assert_eq!(event.body["cwd"], raw["cwd"]);
        assert_eq!(
            event.body["output"],
            sanitize_external_text(raw["output"].as_str().unwrap())
        );
        assert_eq!(event.body["metrics"], raw["metrics"]);
        assert!(
            event.body["nested"][0]
                .as_object()
                .unwrap()
                .values()
                .all(|value| value == "[REDACTED]")
        );
        assert_eq!(
            event.ingest_key,
            event_ingest_key(
                "native",
                0,
                "post-tool-use",
                &serde_json::to_vec(&event.body).unwrap()
            )
        );
        let serialized = serde_json::to_string(&event).unwrap();
        assert!(
            event.body["key_forms"][0]
                .as_object()
                .unwrap()
                .values()
                .all(|value| value == "[REDACTED]")
        );
        assert!(
            event.body["plural_forms"][0]
                .as_object()
                .unwrap()
                .values()
                .all(|value| value == "[REDACTED]")
        );
        assert!(
            event.body["numeric_cases"]
                .as_object()
                .unwrap()
                .values()
                .all(|value| value == "[REDACTED]")
        );
        assert!(event.body["controls"] == raw["controls"]);
        assert!(
            serde_json::to_vec(&event.body["metrics"]).unwrap()
                == serde_json::to_vec(&raw["metrics"]).unwrap()
        );
        assert!(!serialized.contains("abcdefghijklmnop"));
        ai_memory_client::check_body(&event.body).unwrap();
        let expected = event.body.clone();
        let server = thread::spawn(move || {
            let mut bodies = Vec::new();
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                let bytes = read_http_request(&mut socket).1;
                let wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(wire[0]["body"], expected);
                bodies.push(bytes);
                if attempt == 1 {
                    respond_json(&mut socket, "200 OK", r#"{"accepted":1}"#);
                }
            }
            assert_eq!(bodies[0], bodies[1]);
        });
        let client = ImportClient::new(&base).unwrap();
        assert!(client.post_hook_batch(&[event]).await.is_err());
        let retry = planned();
        assert_eq!(serde_json::to_string(&retry).unwrap(), serialized);
        assert_eq!(
            client.post_hook_batch(&[retry]).await.unwrap().ack.accepted,
            1
        );
        server.join().unwrap();
    }

    #[test]
    fn sanitized_redaction_markers_survive_utf8_byte_caps_in_bodies_and_titles() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        for (role, cap) in [
            ("user", MAX_USER_MESSAGE_BYTES),
            ("assistant", MAX_EXTENSION_MESSAGE_BYTES),
        ] {
            let prefix = format!("{}{} token=", "界", "x".repeat(cap - 16));
            let source = serde_json::json!({"project":"p","source":"chatgpt","session_id":"native", "messages":[
                {"role":"user", "content":"clean control"},
                {"role":role, "content":format!("{prefix}abcdefghijklmnop\u{1b}[0m\u{202e}")},
                {"role":"assistant", "content":format!("{} token=abcdefghijklmnop", "x".repeat(145))}
            ]});
            let file = write_conversation(root, source);
            let plan = plan_external_conversation(&file, "w", DEFAULT_SERVER_URL).unwrap();
            for event in &plan.events {
                ai_memory_client::check_body(&event.body).unwrap();
                for field in ["prompt", "message", "title"] {
                    if let Some(text) = event.body[field].as_str() {
                        assert!(!text.contains("abcdefghijklmnop"));
                        assert!(!text.contains(['\u{1b}', '\u{202e}']));
                        if let Some(at) = text.rfind('[') {
                            assert!(
                                text[at..].contains(']'),
                                "redaction marker must remain whole"
                            );
                        }
                    }
                }
            }
            let body = &plan.events[2].body;
            let text = body[if role == "user" { "prompt" } else { "message" }]
                .as_str()
                .unwrap();
            assert!(text.len() <= cap);
            assert!(plan.events[3].body["title"].as_str().unwrap().len() <= 160);
            let again = plan_external_conversation(&file, "w", DEFAULT_SERVER_URL).unwrap();
            assert_eq!(plan.transcript_sha256, again.transcript_sha256);
            assert_eq!(plan.events[2].ingest_key, again.events[2].ingest_key);
        }
    }

    #[tokio::test]
    async fn sensitive_native_identity_is_refused_before_manifest_and_http_with_unicode_control() {
        let root = tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut control = sample_conversation();
        control["session_id"] = " sessão-界-opaque ".into();
        let file = write_conversation(root.path(), control.clone());
        let plan = plan_external_conversation(&file, "w", &base).unwrap();
        assert_eq!(plan.external_session_id, " sessão-界-opaque ");
        assert_eq!(
            plan.events[0].body["external_session_id"],
            control["session_id"]
        );
        assert_eq!(
            plan.stable_session_id,
            stable_external_session_id("w", "memory-lab", "chatgpt", " sessão-界-opaque ")
        );
        for (field, value) in [
            ("project", "sk-1234567890abcdefghijklmnop"),
            ("source", "sk-1234567890abcdefghijklmnop"),
            ("session_id", "sk-1234567890abcdefghijklmnop"),
            ("session_id", "\tnative"),
            ("session_id", "native\n"),
            ("session_id", "\rnative"),
        ] {
            let mut source = control.clone();
            source[field] = value.into();
            let file = write_conversation(root.path(), source);
            assert!(
                plan_external_conversation(&file, "w", &base).is_err(),
                "sensitive native identity must be refused before planning"
            );
            let manifest = root.path().join(format!("{field}-manifest.json"));
            let error = run_conversation(ConversationArgs {
                file,
                workspace: Some("w".into()),
                server_url: base.clone(),
                apply: true,
                manifest_out: Some(manifest.clone()),
                create_destination: false,
                show_body: true,
            })
            .await;
            assert!(error.is_err(), "sensitive source identity must be refused");
            assert!(!error.unwrap_err().to_string().contains("abcdefghijklmnop"));
            assert!(
                !manifest.exists(),
                "refused identity cannot create a manifest"
            );
            assert!(
                listener.accept().is_err(),
                "refused identity cannot reach HTTP"
            );
        }
        let mut oversized = control.clone();
        oversized["session_id"] = format!("native{}", " ".repeat(256)).into();
        let oversized_file = write_conversation(root.path(), oversized);
        assert!(
            plan_external_conversation(&oversized_file, "w", &base).is_err(),
            "native ID bounds apply before trimming"
        );
        let file = write_conversation(root.path(), control);
        let manifest = root.path().join("control-manifest.json");
        run_conversation(ConversationArgs {
            file,
            workspace: Some("w".into()),
            server_url: base,
            apply: false,
            manifest_out: Some(manifest.clone()),
            create_destination: false,
            show_body: true,
        })
        .await
        .unwrap();
        assert!(manifest.exists());
        assert!(listener.accept().is_err());
    }

    #[tokio::test]
    async fn preview_and_manifest_use_scrubbed_bodies_and_digest() {
        let root = tempdir().unwrap();
        let mut source = sample_conversation();
        source["messages"][1]["content"] =
            "界 token=abcdefghijklmnop[REDACTED] literal [REDACTED PRIVATE KEY]".into();
        let file = write_conversation(root.path(), source.clone());
        let plan = plan_external_conversation(&file, "w", DEFAULT_SERVER_URL).unwrap();
        let preview = serde_json::to_string(&plan.events).unwrap();
        assert!(!preview.contains("abcdefghijklmnop"));
        assert!(preview.contains("literal [REDACTED PRIVATE KEY]"));
        source["messages"][1]["content"] =
            sanitize_external_text(source["messages"][1]["content"].as_str().unwrap()).into();
        let safe_file = root.path().join("safe-conversation.json");
        fs::write(&safe_file, serde_json::to_vec(&source).unwrap()).unwrap();
        let safe = plan_external_conversation(&safe_file, "w", DEFAULT_SERVER_URL).unwrap();
        assert_eq!(plan.transcript_sha256, safe.transcript_sha256);
        assert_eq!(plan.events[2].ingest_key, safe.events[2].ingest_key);
        let manifest = root.path().join("manifest.json");
        run_conversation(ConversationArgs {
            file,
            workspace: Some("w".into()),
            server_url: DEFAULT_SERVER_URL.into(),
            apply: false,
            manifest_out: Some(manifest.clone()),
            create_destination: false,
            show_body: true,
        })
        .await
        .unwrap();
        let stored = fs::read_to_string(manifest).unwrap();
        assert!(!stored.contains("abcdefghijklmnop"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&stored).unwrap()["transcript_sha256"],
            plan.transcript_sha256
        );
    }

    #[tokio::test]
    async fn forged_body_authority_is_refused_before_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let clean = planned_hook_event(
            &base,
            "w",
            "p",
            "sessão-界",
            0,
            "session-start",
            None,
            None,
            serde_json::json!({"session_id":"sessão-界"}),
        )
        .unwrap();
        let mut client = ImportClient::new(&base).unwrap();
        client.client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        for field in ["workspace", "project", "actor", "author_id", "headers"] {
            let mut attack = clean.clone();
            attack.body[field] = "forged".into();
            assert!(client.post_hook_batch(&[attack]).await.is_err());
            assert!(
                listener.accept().is_err(),
                "forged body authority must be refused before POST"
            );
        }
        ai_memory_client::check_body(&clean.body).unwrap();
    }
}
