//! `agentmemory-export <file.json>`: import the `ExportData` JSON agentmemory's
//! `export` command writes. File input only: the importer never contacts an
//! agentmemory server, so it never handles that server's credentials.
//!
//! - Every record gets a stable ingest key (a UUIDv5 of its agentmemory id).
//! - A required mapping file names the ai-memory `workspace`/`project` for
//!   each agentmemory project. agentmemory keys projects by directory
//!   basename, which collides across repositories, so the mapping can also
//!   key by exact session `cwd`. Any record left unmapped makes `--apply`
//!   refuse to run.
//! - Memory versions are written oldest first to one path, each conditional on
//!   the version before it, so ai-memory builds its own supersession chain.
//! - `forgetAfter` becomes `expires_at`.
//! - Sessions and their summaries become `<prefix>/sessions/<id>.md`.
//! - Compressed observations are dropped unless `--replay-observations`
//!   replays them as `external-import` sessions through `/hook/batch`, where
//!   the server sanitizer applies again.
//! - Everything without an ai-memory equivalent (graph, actions, signals,
//!   routines, ...) is dropped and counted in the manifest.
//! - All text is redacted client-side before the plan, the manifest or any
//!   request exists.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{
    DEFAULT_DEST_PREFIX, Destination, HookSessionEntry, MAX_EXPLICIT_TITLE_CHARS, MAX_TITLE_CHARS,
    Manifest, PlannedPage, PlannedVersion, RunStatus, apply_pages, display_session_id, ingest_key,
    link_target, memory_kind_tier, one_line, preflight, print_done, print_plan, provenance_line,
    tags_from, validate_dest_prefix, validate_id, validate_planned_path,
};
use crate::{
    DEFAULT_SERVER_URL, ImportClient, MAX_EXTENSION_MESSAGE_BYTES, PlannedHookEvent,
    planned_hook_event, sha256_hex, slugify, stable_external_session_id, truncate_utf8,
    validate_label,
};

const IMPORT_VERSION: &str = "agentmemory-export-v1";
/// The `source` every replayed observation session names.
const SOURCE: &str = "agentmemory";
/// Bound on the export file.
const MAX_EXPORT_BYTES: u64 = 256 * 1024 * 1024;
/// Bound on the mapping file.
const MAX_MAPPING_BYTES: u64 = 1024 * 1024;
/// One hook batch holds at most 256 items: a session start, its
/// observations, and a session end.
const MAX_REPLAYED_OBSERVATIONS: usize = 254;
/// Bound on a replayed observation title, in bytes.
const MAX_OBSERVATION_TITLE_BYTES: usize = 160;

/// Top-level keys this route reads. Every other key is dropped and counted.
const READ_KEYS: &[&str] = &[
    "version",
    "exportedAt",
    "pagination",
    "sessions",
    "observations",
    "memories",
    "summaries",
    "lessons",
    "insights",
    "semanticMemories",
    "proceduralMemories",
    "crystals",
];

#[derive(Parser, Debug, Clone)]
pub(crate) struct ExportArgs {
    /// The `ExportData` JSON file agentmemory's `export` wrote.
    #[arg(long)]
    pub(crate) file: PathBuf,
    /// JSON file mapping agentmemory projects to ai-memory workspace/project.
    #[arg(long)]
    pub(crate) mapping: PathBuf,
    /// Wiki path prefix every imported page lands under.
    #[arg(long, default_value = DEFAULT_DEST_PREFIX)]
    pub(crate) dest_prefix: String,
    #[arg(long, env = "AI_MEMORY_SERVER_URL", default_value = DEFAULT_SERVER_URL)]
    pub(crate) server_url: String,
    /// Write the pages. Without this flag the command only prints a plan.
    #[arg(long)]
    pub(crate) apply: bool,
    /// Durable import manifest. Required with --apply.
    #[arg(long)]
    pub(crate) manifest_out: Option<PathBuf>,
    /// Let the first write create a missing destination workspace/project.
    #[arg(long)]
    pub(crate) create_destination: bool,
    /// Print every planned page body during the dry run.
    #[arg(long)]
    pub(crate) show_body: bool,
    /// Replay compressed observations as `external-import` sessions through
    /// `/hook/batch` instead of dropping them.
    #[arg(long)]
    pub(crate) replay_observations: bool,
    /// Import an export page whose `pagination.hasMore` says more follows.
    #[arg(long)]
    pub(crate) allow_partial: bool,
}

/// The mapping file: agentmemory project basename (and, optionally, exact
/// session cwd) to ai-memory workspace/project.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MappingFile {
    #[serde(default)]
    projects: BTreeMap<String, Target>,
    #[serde(default)]
    cwds: BTreeMap<String, Target>,
    /// Destination for records that name no project at all.
    #[serde(default)]
    unscoped: Option<Target>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    workspace: String,
    project: String,
}

pub(crate) struct Mapping {
    projects: BTreeMap<String, Destination>,
    cwds: BTreeMap<String, Destination>,
    unscoped: Option<Destination>,
}

const NO_PROJECT: &str = "(no project)";

impl Mapping {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let meta =
            fs::metadata(path).with_context(|| format!("read mapping {}", path.display()))?;
        if !meta.is_file() || meta.len() > MAX_MAPPING_BYTES {
            bail!("--mapping must be a regular file of at most {MAX_MAPPING_BYTES} bytes");
        }
        let raw: MappingFile = serde_json::from_slice(&fs::read(path)?)
            .map_err(|e| anyhow!("parse mapping file: {e}"))?;
        let convert = |target: Target| -> Result<Destination> {
            Ok(Destination {
                workspace: validate_label(&target.workspace, "mapping workspace", 128)?.to_owned(),
                project: validate_label(&target.project, "mapping project", 128)?.to_owned(),
                project_id: None,
            })
        };
        Ok(Self {
            projects: raw
                .projects
                .into_iter()
                .map(|(k, v)| Ok((k, convert(v)?)))
                .collect::<Result<_>>()?,
            cwds: raw
                .cwds
                .into_iter()
                .map(|(k, v)| Ok((k, convert(v)?)))
                .collect::<Result<_>>()?,
            unscoped: raw.unscoped.map(convert).transpose()?,
        })
    }

    /// The destination for a record, or the label of the unmapped project.
    fn resolve(&self, project: Option<&str>, cwd: Option<&str>) -> Result<Destination, String> {
        if let Some(found) = cwd.filter(|c| !c.is_empty()).and_then(|c| self.cwds.get(c)) {
            return Ok(found.clone());
        }
        match project.map(str::trim).filter(|p| !p.is_empty()) {
            Some(project) => self
                .projects
                .get(project)
                .cloned()
                .ok_or_else(|| project.to_owned()),
            None => self.unscoped.clone().ok_or_else(|| NO_PROJECT.to_owned()),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    id: String,
    #[serde(default)]
    project: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    started_at: String,
    #[serde(default)]
    ended_at: Option<String>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    observation_count: u64,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    first_prompt: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    commit_shas: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Observation {
    id: String,
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(default)]
    facts: Vec<String>,
    #[serde(default)]
    narrative: String,
    #[serde(default)]
    files: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Memory {
    id: String,
    #[serde(default)]
    created_at: String,
    #[serde(rename = "type", default)]
    memory_type: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    concepts: Vec<String>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    session_ids: Vec<String>,
    #[serde(default)]
    version: Option<u64>,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    supersedes: Vec<String>,
    #[serde(default)]
    related_ids: Vec<String>,
    #[serde(default)]
    forget_after: Option<String>,
    #[serde(default)]
    project: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    session_id: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    narrative: String,
    #[serde(default)]
    key_decisions: Vec<String>,
    #[serde(default)]
    files_modified: Vec<String>,
    #[serde(default)]
    concepts: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lesson {
    id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    context: String,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    reinforcements: Option<u64>,
    #[serde(default)]
    source_ids: Vec<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    deleted: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Insight {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    reinforcements: Option<u64>,
    #[serde(default)]
    source_memory_ids: Vec<String>,
    #[serde(default)]
    source_lesson_ids: Vec<String>,
    #[serde(default)]
    source_crystal_ids: Vec<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    deleted: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SemanticMemory {
    id: String,
    #[serde(default)]
    fact: String,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    source_session_ids: Vec<String>,
    #[serde(default)]
    source_memory_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProceduralMemory {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    steps: Vec<String>,
    #[serde(default)]
    trigger_condition: String,
    #[serde(default)]
    expected_outcome: Option<String>,
    #[serde(default)]
    source_session_ids: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    concepts: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Crystal {
    id: String,
    #[serde(default)]
    narrative: String,
    #[serde(default)]
    key_outcomes: Vec<String>,
    #[serde(default)]
    files_affected: Vec<String>,
    #[serde(default)]
    lessons: Vec<String>,
    #[serde(default)]
    source_action_ids: Vec<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    project: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pagination {
    #[serde(default)]
    has_more: bool,
}

/// One replayed observation session.
#[derive(Debug, Clone)]
pub(crate) struct HookSession {
    source_session_id: String,
    stable_session_id: String,
    destination: Destination,
    events: Vec<PlannedHookEvent>,
}

#[derive(Debug)]
pub(crate) struct ExportPlan {
    pages: Vec<PlannedPage>,
    hook_sessions: Vec<HookSession>,
    dropped: BTreeMap<String, usize>,
    unmapped: BTreeMap<String, usize>,
    partial: bool,
    source_sha256: String,
}

pub(crate) async fn run(args: ExportArgs) -> Result<()> {
    if args.apply && args.manifest_out.is_none() {
        bail!("--apply requires --manifest-out <path>");
    }
    let prefix = validate_dest_prefix(&args.dest_prefix)?;
    let mapping = Mapping::load(&args.mapping)?;
    let plan = plan(
        &args.file,
        &mapping,
        &prefix,
        args.replay_observations,
        &args.server_url,
    )?;
    let mut manifest = Manifest::new(
        IMPORT_VERSION,
        args.file.to_string_lossy().into_owned(),
        plan.source_sha256.clone(),
        &plan.pages,
    );
    manifest.dropped = plan.dropped.clone();
    manifest.unmapped_projects = plan.unmapped.clone();
    manifest.hook_sessions = plan
        .hook_sessions
        .iter()
        .map(|session| HookSessionEntry {
            source_session_id: session.source_session_id.clone(),
            stable_session_id: session.stable_session_id.clone(),
            workspace: session.destination.workspace.clone(),
            project: session.destination.project.clone(),
            planned_events: session.events.len(),
            accepted_events: 0,
            status: RunStatus::Planned,
        })
        .collect();

    if !args.apply {
        print_plan(&plan.pages, &manifest, args.show_body);
        for session in &plan.hook_sessions {
            println!(
                "  replay session {} -> {}/{} ({} hook events)",
                display_session_id(&session.source_session_id),
                session.destination.workspace,
                session.destination.project,
                session.events.len()
            );
        }
        if plan.partial {
            println!("  this export page says more data follows; --apply needs --allow-partial");
        }
        if let Some(path) = &args.manifest_out {
            manifest.write(path)?;
        }
        return Ok(());
    }

    let manifest_path = args
        .manifest_out
        .as_deref()
        .ok_or_else(|| anyhow!("--apply requires --manifest-out <path>"))?;
    // Refusals that need no server come before the client exists.
    if !plan.unmapped.is_empty() {
        let names: Vec<_> = plan.unmapped.keys().map(String::as_str).collect();
        let error = anyhow!(
            "the mapping file does not cover agentmemory project(s) {names:?}; add them (or `unscoped`) and rerun"
        );
        return Err(manifest.fail(manifest_path, error));
    }
    if plan.partial && !args.allow_partial {
        let error = anyhow!(
            "this export page says more data follows (pagination.hasMore); export everything or pass --allow-partial"
        );
        return Err(manifest.fail(manifest_path, error));
    }
    manifest.write(manifest_path)?;

    let client = ImportClient::new(&args.server_url)?;
    let destinations: BTreeSet<Destination> = plan
        .pages
        .iter()
        .map(|page| page.destination.clone())
        .chain(plan.hook_sessions.iter().map(|s| s.destination.clone()))
        .collect();
    if let Err(error) = preflight(&client, &destinations, args.create_destination).await {
        return Err(manifest.fail(manifest_path, error));
    }
    apply_pages(&client, &plan.pages, &mut manifest, manifest_path).await?;
    for (index, session) in plan.hook_sessions.iter().enumerate() {
        let delivery = match client.post_hook_batch(&session.events).await {
            Ok(delivery) => delivery,
            Err(error) => return Err(manifest.fail(manifest_path, error)),
        };
        let entry = &mut manifest.hook_sessions[index];
        entry.accepted_events = delivery.ack.accepted_count();
        if !delivery.status.is_success() || !delivery.ack.accepted_every_event(session.events.len())
        {
            entry.status = RunStatus::Failed;
            let error = anyhow!(
                "hook batch for session {} returned HTTP {} and accepted {} of {} events; rerun to resume",
                session.source_session_id,
                delivery.status,
                entry.accepted_events,
                session.events.len()
            );
            return Err(manifest.fail(manifest_path, error));
        }
        entry.status = RunStatus::Imported;
        manifest.write(manifest_path)?;
    }
    manifest.status = RunStatus::Imported;
    manifest.write(manifest_path)?;
    print_done(&manifest);
    if !plan.hook_sessions.is_empty() {
        println!(
            "replayed {} observation session(s) through /hook/batch",
            plan.hook_sessions.len()
        );
    }
    Ok(())
}

/// Parse one array the route reads; a malformed record fails the plan.
fn records<T: DeserializeOwned>(
    export: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Vec<T>> {
    match export.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                serde_json::from_value(item.clone())
                    .map_err(|e| anyhow!("{key}[{index}] is not a valid agentmemory record: {e}"))
            })
            .collect(),
        Some(_) => bail!("{key} must be an array"),
    }
}

/// How many records a dropped top-level value holds.
fn record_count(value: &Value) -> usize {
    match value {
        Value::Array(items) => items.len(),
        Value::Object(map) => map.len(),
        Value::Null => 0,
        _ => 1,
    }
}

/// What the mapping file is consulted with for one record.
struct Scope {
    project: Option<String>,
    cwd: Option<String>,
}

fn refs(ids: &[String]) -> Vec<&str> {
    ids.iter().map(String::as_str).collect()
}

/// A loose RFC 3339 / `YYYY-MM-DD` shape check; the server parses it exactly.
fn looks_like_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    let date = bytes.len() >= 10
        && bytes[..10].iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        });
    date && (bytes.len() == 10 || (bytes.len() <= 40 && bytes[10] == b'T'))
}

/// Pages and replay sessions grouped per destination, plus the link table.
struct Builder<'a> {
    prefix: &'a str,
    pages: Vec<PlannedPage>,
    paths: HashMap<String, String>,
    /// agentmemory id → (destination, path) of the imported page.
    links: HashMap<String, (Destination, String)>,
    unmapped: BTreeMap<String, usize>,
}

impl Builder<'_> {
    fn path(&mut self, folder: &str, id: &str) -> Result<String> {
        let path = format!("{}/{folder}/{}.md", self.prefix, slugify(id));
        validate_planned_path(&path, self.prefix)?;
        if let Some(other) = self.paths.insert(path.clone(), id.to_owned()) {
            bail!("duplicate destination path collision: {other} and {id} both map to {path}");
        }
        Ok(path)
    }

    fn unmapped(&mut self, label: String, count: usize) {
        *self.unmapped.entry(label).or_default() += count;
    }

    /// A rendered reference to another agentmemory record: a wikilink when
    /// it was imported (scoped when it landed in another project), plain text
    /// otherwise.
    fn link(&self, from: &Destination, id: &str) -> String {
        match self.links.get(id) {
            Some((to, path)) if to == from => format!("[[{}]]", link_target(path)),
            Some((to, path)) => {
                format!("[[{}/{}:{}]]", to.workspace, to.project, link_target(path))
            }
            None => format!("`{id}`"),
        }
    }
}

fn bullet_list(out: &mut String, heading: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("\n## {heading}\n"));
    for item in items {
        out.push_str(&format!("- {item}\n"));
    }
}

fn code_list(out: &mut String, heading: &str, items: &[String]) {
    let items: Vec<String> = items.iter().map(|item| format!("`{item}`")).collect();
    bullet_list(out, heading, &items);
}

/// Sanitize an assembled body and close it with its provenance line.
fn finish_body(body: &str, record: &str, id: &str, version: Option<u64>, key: &str) -> String {
    format!(
        "{}\n\n---\n{}\n",
        ai_memory_client::sanitize_external_text(body).trim_end(),
        provenance_line(record, id, version, key)
    )
}

fn single_version(
    record: &'static str,
    id: &str,
    title: String,
    body: &str,
    kind: &str,
    tier: &'static str,
    tags: &[String],
) -> PlannedVersion {
    let key = ingest_key(record, id);
    PlannedVersion {
        body: finish_body(body, record, id, None, &key),
        key,
        source_id: id.to_owned(),
        version: None,
        title: ai_memory_client::sanitize_external_text(&title),
        kind: kind.to_owned(),
        tier,
        tags: tags_from(tags.iter().map(String::as_str)),
        expires_at: None,
    }
}

/// Read, validate, map and plan a whole export. No network, no writes.
pub(crate) fn plan(
    file: &Path,
    mapping: &Mapping,
    prefix: &str,
    replay_observations: bool,
    server_url: &str,
) -> Result<ExportPlan> {
    let meta = fs::metadata(file).with_context(|| format!("read export {}", file.display()))?;
    if !meta.is_file() {
        bail!("--file must be a regular file");
    }
    if meta.len() > MAX_EXPORT_BYTES {
        bail!(
            "export file is {} bytes; maximum is {MAX_EXPORT_BYTES}",
            meta.len()
        );
    }
    let bytes = fs::read(file).with_context(|| format!("read {}", file.display()))?;
    let source_sha256 = sha256_hex(&bytes);
    let export: Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow!("parse agentmemory export JSON: not valid JSON"))?;
    let Value::Object(export) = export else {
        bail!("an agentmemory export is a JSON object");
    };

    let mut dropped: BTreeMap<String, usize> = BTreeMap::new();
    for (key, value) in &export {
        if !READ_KEYS.contains(&key.as_str()) {
            let count = record_count(value);
            if count > 0 {
                dropped.insert(key.clone(), count);
            }
        }
    }
    let partial = match export.get("pagination") {
        Some(value) if !value.is_null() => {
            serde_json::from_value::<Pagination>(value.clone())
                .map_err(|e| anyhow!("pagination is malformed: {e}"))?
                .has_more
        }
        _ => false,
    };

    let sessions: Vec<Session> = records(&export, "sessions")?;
    let memories: Vec<Memory> = records(&export, "memories")?;
    let summaries: Vec<Summary> = records(&export, "summaries")?;
    let lessons: Vec<Lesson> = records(&export, "lessons")?;
    let insights: Vec<Insight> = records(&export, "insights")?;
    let semantic: Vec<SemanticMemory> = records(&export, "semanticMemories")?;
    let procedural: Vec<ProceduralMemory> = records(&export, "proceduralMemories")?;
    let crystals: Vec<Crystal> = records(&export, "crystals")?;
    let observations: BTreeMap<String, Vec<Observation>> = match export.get("observations") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(session, items)| {
                let parsed: Vec<Observation> = serde_json::from_value(items.clone())
                    .map_err(|e| anyhow!("observations[{session:?}] is malformed: {e}"))?;
                Ok((session.clone(), parsed))
            })
            .collect::<Result<_>>()?,
        Some(_) => bail!("observations must be an object keyed by session id"),
    };

    let mut by_session: HashMap<&str, &Session> = HashMap::new();
    for session in &sessions {
        validate_id(&session.id, "session")?;
        if by_session.insert(session.id.as_str(), session).is_some() {
            bail!("duplicate session id {:?}", session.id);
        }
    }
    // A record's mapping key: its own project, else the project of the first
    // session it cites, plus that session's cwd, so a cwd mapping follows
    // every record tied to the session.
    let scope_for = |project: Option<&'_ str>, session_ids: &[&str]| -> Scope {
        let session = session_ids
            .iter()
            .find_map(|id| by_session.get(id).copied());
        Scope {
            project: project
                .filter(|p| !p.trim().is_empty())
                .or_else(|| session.map(|s| s.project.as_str()))
                .map(str::to_owned),
            cwd: session.map(|s| s.cwd.clone()),
        }
    };

    let mut builder = Builder {
        prefix,
        pages: Vec::new(),
        paths: HashMap::new(),
        links: HashMap::new(),
        unmapped: BTreeMap::new(),
    };

    // Memories: group versions into lineages through parentId/supersedes.
    let mut memory_index: HashMap<&str, usize> = HashMap::new();
    for (index, memory) in memories.iter().enumerate() {
        validate_id(&memory.id, "memory")?;
        if memory_index.insert(memory.id.as_str(), index).is_some() {
            bail!("duplicate memory id {:?}", memory.id);
        }
        if let Some(ttl) = memory.forget_after.as_deref().filter(|t| !t.is_empty())
            && !looks_like_timestamp(ttl)
        {
            bail!(
                "memory {:?} has a forgetAfter that is not a timestamp",
                memory.id
            );
        }
    }
    let mut parent: Vec<usize> = (0..memories.len()).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for (index, memory) in memories.iter().enumerate() {
        for other in memory.parent_id.iter().chain(&memory.supersedes) {
            if let Some(&other) = memory_index.get(other.as_str()) {
                let (a, b) = (find(&mut parent, index), find(&mut parent, other));
                parent[a] = b;
            }
        }
    }
    let mut lineages: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in 0..memories.len() {
        let root = find(&mut parent, index);
        lineages.entry(root).or_default().push(index);
    }
    let mut lineages: Vec<Vec<usize>> = lineages.into_values().collect();
    for lineage in &mut lineages {
        lineage.sort_by(|&a, &b| {
            let (a, b) = (&memories[a], &memories[b]);
            (a.version.unwrap_or(1), &a.created_at, &a.id).cmp(&(
                b.version.unwrap_or(1),
                &b.created_at,
                &b.id,
            ))
        });
    }
    // Oldest lineage first keeps the plan order stable across runs.
    lineages.sort_by(|a, b| memories[a[0]].id.cmp(&memories[b[0]].id));

    let mut memory_plans = Vec::new();
    for lineage in &lineages {
        let newest = &memories[*lineage.last().unwrap_or(&lineage[0])];
        let scope = scope_for(newest.project.as_deref(), &refs(&newest.session_ids));
        match mapping.resolve(scope.project.as_deref(), scope.cwd.as_deref()) {
            Ok(destination) => {
                let path = builder.path("memories", &memories[lineage[0]].id)?;
                for &index in lineage {
                    builder.links.insert(
                        memories[index].id.clone(),
                        (destination.clone(), path.clone()),
                    );
                }
                memory_plans.push((lineage, destination, path));
            }
            Err(label) => builder.unmapped(label, lineage.len()),
        }
    }

    // Single-version records: resolve, reserve a path and a link first, so
    // bodies can link to each other.
    struct Single<'a> {
        record: &'static str,
        id: &'a str,
        destination: Destination,
        path: String,
    }
    let mut singles: Vec<Single> = Vec::new();
    let reserve = |builder: &mut Builder,
                   record: &'static str,
                   folder: &str,
                   id: &str,
                   scope: Scope|
     -> Result<Option<(Destination, String)>> {
        validate_id(id, record)?;
        match mapping.resolve(scope.project.as_deref(), scope.cwd.as_deref()) {
            Ok(destination) => {
                let path = builder.path(folder, id)?;
                builder
                    .links
                    .insert(id.to_owned(), (destination.clone(), path.clone()));
                Ok(Some((destination, path)))
            }
            Err(label) => {
                builder.unmapped(label, 1);
                Ok(None)
            }
        }
    };
    let mut lessons_deleted = 0usize;
    for lesson in &lessons {
        if lesson.deleted == Some(true) {
            lessons_deleted += 1;
            continue;
        }
        let scope = scope_for(lesson.project.as_deref(), &[]);
        if let Some((destination, path)) =
            reserve(&mut builder, "lesson", "lessons", &lesson.id, scope)?
        {
            singles.push(Single {
                record: "lesson",
                id: &lesson.id,
                destination,
                path,
            });
        }
    }
    let mut insights_deleted = 0usize;
    for insight in &insights {
        if insight.deleted == Some(true) {
            insights_deleted += 1;
            continue;
        }
        let scope = scope_for(insight.project.as_deref(), &[]);
        if let Some((destination, path)) =
            reserve(&mut builder, "insight", "insights", &insight.id, scope)?
        {
            singles.push(Single {
                record: "insight",
                id: &insight.id,
                destination,
                path,
            });
        }
    }
    for fact in &semantic {
        let scope = scope_for(None, &refs(&fact.source_session_ids));
        if let Some((destination, path)) =
            reserve(&mut builder, "semantic-memory", "semantic", &fact.id, scope)?
        {
            singles.push(Single {
                record: "semantic-memory",
                id: &fact.id,
                destination,
                path,
            });
        }
    }
    for procedure in &procedural {
        let scope = scope_for(None, &refs(&procedure.source_session_ids));
        if let Some((destination, path)) = reserve(
            &mut builder,
            "procedural-memory",
            "procedural",
            &procedure.id,
            scope,
        )? {
            singles.push(Single {
                record: "procedural-memory",
                id: &procedure.id,
                destination,
                path,
            });
        }
    }
    for crystal in &crystals {
        let scope = scope_for(
            crystal.project.as_deref(),
            &crystal
                .session_id
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
        if let Some((destination, path)) =
            reserve(&mut builder, "crystal", "crystals", &crystal.id, scope)?
        {
            singles.push(Single {
                record: "crystal",
                id: &crystal.id,
                destination,
                path,
            });
        }
    }
    // Sessions and summaries share one page per session id.
    let mut summary_by_session: HashMap<&str, &Summary> = HashMap::new();
    for summary in &summaries {
        validate_id(&summary.session_id, "summary session")?;
        summary_by_session.insert(summary.session_id.as_str(), summary);
    }
    let mut session_ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    for summary in &summaries {
        if !by_session.contains_key(summary.session_id.as_str()) {
            session_ids.push(summary.session_id.as_str());
        }
    }
    let mut session_destinations: HashMap<&str, Destination> = HashMap::new();
    for &id in &session_ids {
        // A session record's own project wins over its summary's.
        let project = by_session.get(id).map(|s| s.project.as_str()).or_else(|| {
            summary_by_session
                .get(id)
                .and_then(|s| s.project.as_deref())
        });
        let scope = scope_for(project, &[id]);
        if let Some((destination, path)) = reserve(&mut builder, "session", "sessions", id, scope)?
        {
            session_destinations.insert(id, destination.clone());
            singles.push(Single {
                record: "session",
                id,
                destination,
                path,
            });
        }
    }
    if lessons_deleted > 0 {
        dropped.insert("lessons (deleted)".into(), lessons_deleted);
    }
    if insights_deleted > 0 {
        dropped.insert("insights (deleted)".into(), insights_deleted);
    }

    // Now render bodies, with every link target known.
    for (lineage, destination, path) in memory_plans {
        let mut versions = Vec::with_capacity(lineage.len());
        for &index in lineage {
            let memory = &memories[index];
            let (kind, tier) = memory_kind_tier(&memory.memory_type);
            let title = if memory.title.trim().is_empty() {
                one_line(&memory.content, MAX_TITLE_CHARS)
            } else {
                one_line(&memory.title, MAX_EXPLICIT_TITLE_CHARS)
            };
            let mut body = format!("# {title}\n\n{}\n", memory.content.trim());
            code_list(&mut body, "Files", &memory.files);
            let related: Vec<String> = memory
                .related_ids
                .iter()
                .filter(|id| !lineage.iter().any(|&i| &memories[i].id == *id))
                .map(|id| builder.link(&destination, id))
                .collect();
            bullet_list(&mut body, "Related", &related);
            let key = ingest_key("memory", &memory.id);
            versions.push(PlannedVersion {
                body: finish_body(&body, "memory", &memory.id, memory.version, &key),
                key,
                source_id: memory.id.clone(),
                version: memory.version,
                title: ai_memory_client::sanitize_external_text(&title),
                kind: kind.to_owned(),
                tier,
                tags: tags_from(memory.concepts.iter().map(String::as_str)),
                expires_at: memory.forget_after.clone().filter(|t| !t.is_empty()),
            });
        }
        builder.pages.push(PlannedPage {
            record: "memory",
            destination,
            path,
            versions,
        });
    }

    let lesson_by_id: HashMap<&str, &Lesson> = lessons.iter().map(|l| (l.id.as_str(), l)).collect();
    let insight_by_id: HashMap<&str, &Insight> =
        insights.iter().map(|i| (i.id.as_str(), i)).collect();
    let semantic_by_id: HashMap<&str, &SemanticMemory> =
        semantic.iter().map(|s| (s.id.as_str(), s)).collect();
    let procedural_by_id: HashMap<&str, &ProceduralMemory> =
        procedural.iter().map(|p| (p.id.as_str(), p)).collect();
    let crystal_by_id: HashMap<&str, &Crystal> =
        crystals.iter().map(|c| (c.id.as_str(), c)).collect();
    for single in &singles {
        let to = &single.destination;
        let links =
            |ids: &[String]| -> Vec<String> { ids.iter().map(|id| builder.link(to, id)).collect() };
        let version = match single.record {
            "lesson" => {
                let lesson = lesson_by_id[single.id];
                let title = format!("Lesson: {}", one_line(&lesson.content, MAX_TITLE_CHARS));
                let mut body = format!("# {title}\n\n{}\n", lesson.content.trim());
                if !lesson.context.trim().is_empty() {
                    body.push_str(&format!("\n## Context\n{}\n", lesson.context.trim()));
                }
                body.push_str(&strength_line(lesson.confidence, lesson.reinforcements));
                bullet_list(&mut body, "Sources", &links(&lesson.source_ids));
                single_version(
                    "lesson",
                    single.id,
                    title,
                    &body,
                    "lesson",
                    "semantic",
                    &lesson.tags,
                )
            }
            "insight" => {
                let insight = insight_by_id[single.id];
                let title = if insight.title.trim().is_empty() {
                    one_line(&insight.content, MAX_TITLE_CHARS)
                } else {
                    one_line(&insight.title, MAX_EXPLICIT_TITLE_CHARS)
                };
                let mut body = format!("# {title}\n\n{}\n", insight.content.trim());
                body.push_str(&strength_line(insight.confidence, insight.reinforcements));
                let sources: Vec<String> = insight
                    .source_memory_ids
                    .iter()
                    .chain(&insight.source_lesson_ids)
                    .chain(&insight.source_crystal_ids)
                    .cloned()
                    .collect();
                bullet_list(&mut body, "Sources", &links(&sources));
                single_version(
                    "insight",
                    single.id,
                    title,
                    &body,
                    "insight",
                    "semantic",
                    &insight.tags,
                )
            }
            "semantic-memory" => {
                let fact = semantic_by_id[single.id];
                let title = one_line(&fact.fact, MAX_TITLE_CHARS);
                let mut body = format!("# {title}\n\n{}\n", fact.fact.trim());
                body.push_str(&strength_line(fact.confidence, None));
                bullet_list(&mut body, "Sources", &links(&fact.source_memory_ids));
                single_version(
                    "semantic-memory",
                    single.id,
                    title,
                    &body,
                    "fact",
                    "semantic",
                    &[],
                )
            }
            "procedural-memory" => {
                let procedure = procedural_by_id[single.id];
                let title = one_line(&procedure.name, MAX_EXPLICIT_TITLE_CHARS);
                let mut body = format!("# {title}\n");
                if !procedure.trigger_condition.trim().is_empty() {
                    body.push_str(&format!(
                        "\n**When:** {}\n",
                        procedure.trigger_condition.trim()
                    ));
                }
                if !procedure.steps.is_empty() {
                    body.push_str("\n## Steps\n");
                    for (n, step) in procedure.steps.iter().enumerate() {
                        body.push_str(&format!("{}. {step}\n", n + 1));
                    }
                }
                if let Some(outcome) = procedure
                    .expected_outcome
                    .as_deref()
                    .filter(|o| !o.trim().is_empty())
                {
                    body.push_str(&format!("\n**Expected outcome:** {}\n", outcome.trim()));
                }
                let tags: Vec<String> = procedure
                    .tags
                    .iter()
                    .chain(&procedure.concepts)
                    .cloned()
                    .collect();
                single_version(
                    "procedural-memory",
                    single.id,
                    title,
                    &body,
                    "procedure",
                    "procedural",
                    &tags,
                )
            }
            "crystal" => {
                let crystal = crystal_by_id[single.id];
                let title = format!("Crystal: {}", one_line(&crystal.narrative, MAX_TITLE_CHARS));
                let mut body = format!("# {title}\n\n{}\n", crystal.narrative.trim());
                bullet_list(&mut body, "Key outcomes", &crystal.key_outcomes);
                bullet_list(&mut body, "Lessons", &crystal.lessons);
                code_list(&mut body, "Files", &crystal.files_affected);
                bullet_list(
                    &mut body,
                    "Source actions",
                    &links(&crystal.source_action_ids),
                );
                single_version(
                    "crystal",
                    single.id,
                    title,
                    &body,
                    "crystal",
                    "episodic",
                    &[],
                )
            }
            _ => {
                let session = by_session.get(single.id).copied();
                let summary = summary_by_session.get(single.id).copied();
                let (title, body, tags) = session_page(single.id, session, summary);
                single_version(
                    "session", single.id, title, &body, "session", "episodic", &tags,
                )
            }
        };
        builder.pages.push(PlannedPage {
            record: single.record,
            destination: single.destination.clone(),
            path: single.path.clone(),
            versions: vec![version],
        });
    }

    // Observations: counted as dropped, or planned as replay sessions.
    let total_observations: usize = observations.values().map(Vec::len).sum();
    let mut hook_sessions = Vec::new();
    if !replay_observations {
        if total_observations > 0 {
            dropped.insert("observations".into(), total_observations);
        }
    } else {
        let mut without_session = 0usize;
        let mut over_cap = 0usize;
        for (session_id, items) in &observations {
            if items.is_empty() {
                continue;
            }
            let Some(session) = by_session.get(session_id.as_str()) else {
                without_session += items.len();
                continue;
            };
            let Some(destination) = session_destinations.get(session_id.as_str()) else {
                // The session page is unmapped; its label is already counted.
                *builder
                    .unmapped
                    .entry(
                        mapping
                            .resolve(Some(&session.project), Some(&session.cwd))
                            .err()
                            .unwrap_or_default(),
                    )
                    .or_default() += items.len();
                continue;
            };
            let mut sorted: Vec<&Observation> = items.iter().collect();
            sorted.sort_by(|a, b| (&a.timestamp, &a.id).cmp(&(&b.timestamp, &b.id)));
            if sorted.len() > MAX_REPLAYED_OBSERVATIONS {
                over_cap += sorted.len() - MAX_REPLAYED_OBSERVATIONS;
                sorted.truncate(MAX_REPLAYED_OBSERVATIONS);
            }
            hook_sessions.push(replay_session(server_url, session, destination, &sorted)?);
        }
        if without_session > 0 {
            dropped.insert(
                "observations without a session record".into(),
                without_session,
            );
        }
        if over_cap > 0 {
            dropped.insert(
                "observations over the per-session replay cap".into(),
                over_cap,
            );
        }
    }
    if partial {
        dropped.insert("export pages after this one (pagination.hasMore)".into(), 1);
    }

    Ok(ExportPlan {
        pages: builder.pages,
        hook_sessions,
        dropped,
        unmapped: builder.unmapped,
        partial,
        source_sha256,
    })
}

fn strength_line(confidence: Option<f64>, reinforcements: Option<u64>) -> String {
    match (confidence, reinforcements) {
        (Some(c), Some(r)) => {
            format!("\nConfidence {c:.2}, reinforced {r} time(s) in agentmemory.\n")
        }
        (Some(c), None) => format!("\nConfidence {c:.2} in agentmemory.\n"),
        (None, Some(r)) => format!("\nReinforced {r} time(s) in agentmemory.\n"),
        (None, None) => String::new(),
    }
}

fn session_page(
    id: &str,
    session: Option<&Session>,
    summary: Option<&Summary>,
) -> (String, String, Vec<String>) {
    let title = summary
        .map(|s| s.title.trim())
        .filter(|t| !t.is_empty())
        .map(|t| one_line(t, MAX_EXPLICIT_TITLE_CHARS))
        .or_else(|| {
            session
                .and_then(|s| s.first_prompt.as_deref())
                .filter(|p| !p.trim().is_empty())
                .map(|p| one_line(p, MAX_TITLE_CHARS))
        })
        .unwrap_or_else(|| format!("agentmemory session {}", one_line(id, MAX_TITLE_CHARS)));
    let mut body = format!("# {title}\n\n");
    if let Some(session) = session {
        let mut meta = vec![
            format!("- Project: {}", session.project),
            format!("- Status: {}", session.status),
            format!("- Started: {}", session.started_at),
        ];
        if let Some(ended) = session.ended_at.as_deref() {
            meta.push(format!("- Ended: {ended}"));
        }
        meta.push(format!("- Observations: {}", session.observation_count));
        if !session.cwd.is_empty() {
            meta.push(format!("- Working directory: `{}`", session.cwd));
        }
        if let Some(model) = session.model.as_deref() {
            meta.push(format!("- Model: {model}"));
        }
        body.push_str(&meta.join("\n"));
        body.push('\n');
    }
    if let Some(summary) = summary {
        if !summary.narrative.trim().is_empty() {
            body.push_str(&format!("\n## Summary\n{}\n", summary.narrative.trim()));
        }
        bullet_list(&mut body, "Key decisions", &summary.key_decisions);
        code_list(&mut body, "Files modified", &summary.files_modified);
    } else if let Some(text) = session
        .and_then(|s| s.summary.as_deref())
        .filter(|s| !s.trim().is_empty())
    {
        body.push_str(&format!("\n## Summary\n{}\n", text.trim()));
    }
    if let Some(prompt) = session
        .and_then(|s| s.first_prompt.as_deref())
        .filter(|p| !p.trim().is_empty())
    {
        body.push_str(&format!("\n## First prompt\n{}\n", prompt.trim()));
    }
    if let Some(session) = session {
        code_list(&mut body, "Commits", &session.commit_shas);
    }
    let mut tags: Vec<String> = summary.map(|s| s.concepts.clone()).unwrap_or_default();
    if let Some(session) = session {
        tags.extend(session.tags.iter().cloned());
    }
    (title, body, tags)
}

/// Plan one observation session as an ordered `external-import` hook batch.
fn replay_session(
    server_url: &str,
    session: &Session,
    destination: &Destination,
    observations: &[&Observation],
) -> Result<HookSession> {
    let stable = stable_external_session_id(
        &destination.workspace,
        &destination.project,
        SOURCE,
        &session.id,
    );
    let at = |value: &str| -> Value {
        if looks_like_timestamp(value) {
            json!(value)
        } else {
            Value::Null
        }
    };
    let mut events = Vec::with_capacity(observations.len() + 2);
    let mut start = json!({
        "session_id": stable,
        "model": format!("external:{SOURCE}"),
        "external_source": SOURCE,
        "external_session_id": session.id,
    });
    if let Value::String(ts) = at(&session.started_at) {
        start["occurred_at"] = json!(ts);
    }
    events.push(planned_hook_event(
        server_url,
        &destination.workspace,
        &destination.project,
        &stable,
        0,
        "session-start",
        None,
        None,
        start,
    )?);
    for (n, observation) in observations.iter().enumerate() {
        validate_id(&observation.id, "observation")?;
        let mut text = String::new();
        if let Some(subtitle) = observation
            .subtitle
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            text.push_str(subtitle.trim());
            text.push_str("\n\n");
        }
        text.push_str(observation.narrative.trim());
        for fact in &observation.facts {
            text.push_str(&format!("\n- {fact}"));
        }
        if !observation.files.is_empty() {
            text.push_str(&format!("\nFiles: {}", observation.files.join(", ")));
        }
        let message = truncate_utf8(
            &ai_memory_client::sanitize_external_text(&text),
            MAX_EXTENSION_MESSAGE_BYTES,
        );
        let title = truncate_utf8(
            &ai_memory_client::sanitize_external_text(&one_line(
                &observation.title,
                MAX_TITLE_CHARS,
            )),
            MAX_OBSERVATION_TITLE_BYTES,
        );
        let mut body = json!({
            "session_id": stable,
            "title": if title.is_empty() { "agentmemory observation".to_owned() } else { title },
            "message": if message.trim().is_empty() { "(empty observation)".to_owned() } else { message },
            "external_source": SOURCE,
        });
        if let Value::String(ts) = at(&observation.timestamp) {
            body["occurred_at"] = json!(ts);
        }
        events.push(planned_hook_event(
            server_url,
            &destination.workspace,
            &destination.project,
            &stable,
            n + 1,
            "external.observation",
            None,
            Some("agentmemory-observation"),
            body,
        )?);
    }
    let mut end = json!({
        "session_id": stable,
        "external_source": SOURCE,
        "external_session_id": session.id,
    });
    let ended = session
        .ended_at
        .clone()
        .or_else(|| observations.last().map(|o| o.timestamp.clone()))
        .unwrap_or_default();
    if let Value::String(ts) = at(&ended) {
        end["occurred_at"] = json!(ts);
    }
    events.push(planned_hook_event(
        server_url,
        &destination.workspace,
        &destination.project,
        &stable,
        observations.len() + 1,
        "session-end",
        None,
        None,
        end,
    )?);
    Ok(HookSession {
        source_session_id: session.id.clone(),
        stable_session_id: stable,
        destination: destination.clone(),
        events,
    })
}

#[cfg(test)]
pub(crate) mod tests;
