//! `claude-mem`: import the durable, model-produced observations and session
//! summaries from claude-mem's local SQLite database.
//!
//! The source is snapshotted through SQLite's backup API before inspection, so
//! a live WAL database is read consistently. Schema recognition is deliberately
//! narrow: known legacy and structured observation layouts are accepted after
//! inspecting the actual columns; an unfamiliar layout fails closed.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use rusqlite::backup::{Backup, StepResult};
use rusqlite::limits::Limit;
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use uuid::Uuid;

use crate::agentmemory::{
    Destination, Manifest, PlannedPage, PlannedVersion, apply_pages_batched, one_line, print_done,
    print_plan, tags_from, validate_dest_prefix, validate_planned_path,
};
use crate::mcp::READ_PAGE_MAX_CHARS;
use crate::{DEFAULT_SERVER_URL, ImportClient, validate_label};

const IMPORT_VERSION: &str = "claude-mem-sqlite-v1";
const DEFAULT_SOURCE: &str = "~/.claude-mem/claude-mem.db";
const DEFAULT_DEST_PREFIX: &str = "imported/claude-mem";
const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_MAPPING_BYTES: u64 = 1024 * 1024;
const MAX_ROWS: usize = 100_000;
const MAX_FIELD_BYTES: usize = 1024 * 1024;
const MAX_SQLITE_VALUE_BYTES: i32 = 8 * 1024 * 1024;
const MAX_PLANNED_BODY_BYTES: usize = 512 * 1024 * 1024;
const MAX_PROJECT_BYTES: usize = 512;
const MAX_CWD_BYTES: usize = 4096;
const MAX_TYPE_BYTES: usize = 64;
const MAX_TIMESTAMP_BYTES: usize = 128;

#[derive(Parser, Debug, Clone)]
pub(crate) struct ClaudeMemArgs {
    /// claude-mem SQLite database (defaults to ~/.claude-mem/claude-mem.db).
    #[arg(long, value_name = "PATH")]
    pub(crate) database: Option<PathBuf>,
    /// JSON mapping claude-mem projects/cwds to ai-memory destinations.
    #[arg(long)]
    pub(crate) mapping: PathBuf,
    /// Wiki path prefix every imported page lands under.
    #[arg(long, default_value = DEFAULT_DEST_PREFIX)]
    pub(crate) dest_prefix: String,
    #[arg(long, env = "AI_MEMORY_SERVER_URL", default_value = DEFAULT_SERVER_URL)]
    pub(crate) server_url: String,
    /// Write pages. Without this flag the command only prints a plan.
    #[arg(long)]
    pub(crate) apply: bool,
    /// Durable import manifest. Required with --apply.
    #[arg(long)]
    pub(crate) manifest_out: Option<PathBuf>,
    /// Print sanitized planned page bodies during the dry run.
    #[arg(long)]
    pub(crate) show_body: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MappingFile {
    #[serde(default)]
    projects: BTreeMap<String, Target>,
    #[serde(default)]
    cwds: BTreeMap<String, Target>,
    #[serde(default)]
    unscoped: Option<Target>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    workspace: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
}

struct Mapping {
    projects: BTreeMap<String, Destination>,
    cwds: BTreeMap<String, Destination>,
    unscoped: Option<Destination>,
}

impl Mapping {
    fn load(path: &Path) -> Result<Self> {
        let metadata =
            fs::metadata(path).with_context(|| format!("read mapping {}", path.display()))?;
        if !metadata.is_file() || metadata.len() > MAX_MAPPING_BYTES {
            bail!("--mapping must be a regular file of at most {MAX_MAPPING_BYTES} bytes");
        }
        let raw: MappingFile = serde_json::from_slice(&fs::read(path)?)
            .map_err(|error| anyhow!("parse mapping file: {error}"))?;
        let convert = |target: Target| -> Result<Destination> {
            let project = target
                .project
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let project_id = target
                .project_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            if project.is_some() == project_id.is_some() {
                bail!("each mapping target requires exactly one of project or project_id");
            }
            let project_id = project_id
                .map(|value| {
                    if value.len() > 64 {
                        bail!("mapping project_id is too long");
                    }
                    Uuid::parse_str(value)
                        .map(|id| id.to_string())
                        .map_err(|_| anyhow!("mapping project_id must be a UUID"))
                })
                .transpose()?;
            Ok(Destination {
                workspace: validate_label(&target.workspace, "mapping workspace", 128)?.to_owned(),
                project: project
                    .map(|value| validate_label(value, "mapping project", 128).map(str::to_owned))
                    .transpose()?
                    .unwrap_or_default(),
                project_id,
            })
        };
        Ok(Self {
            projects: raw
                .projects
                .into_iter()
                .map(|(key, value)| Ok((key, convert(value)?)))
                .collect::<Result<_>>()?,
            cwds: raw
                .cwds
                .into_iter()
                .map(|(key, value)| Ok((key, convert(value)?)))
                .collect::<Result<_>>()?,
            unscoped: raw.unscoped.map(convert).transpose()?,
        })
    }

    fn resolve(&self, project: Option<&str>, cwd: Option<&str>) -> Result<Destination, String> {
        if let Some(found) = cwd
            .filter(|value| !value.is_empty())
            .and_then(|value| self.cwds.get(value))
        {
            return Ok(found.clone());
        }
        match project.map(str::trim).filter(|value| !value.is_empty()) {
            Some(project) => self
                .projects
                .get(project)
                .cloned()
                .ok_or_else(|| project.to_owned()),
            None => self
                .unscoped
                .clone()
                .ok_or_else(|| "(no project)".to_owned()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObservationLayout {
    LegacyText,
    Structured,
}

impl ObservationLayout {
    const fn name(self) -> &'static str {
        match self {
            Self::LegacyText => "legacy-text",
            Self::Structured => "structured-v7+",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionJoin {
    Memory,
    Sdk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObservationType {
    Decision,
    Gotcha,
    SecurityNote,
    Feature,
    Bugfix,
    Change,
    Refactor,
    Discovery,
}

impl ObservationType {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "decision" => Some(Self::Decision),
            "gotcha" => Some(Self::Gotcha),
            "security_note" => Some(Self::SecurityNote),
            "feature" => Some(Self::Feature),
            "bugfix" => Some(Self::Bugfix),
            "change" => Some(Self::Change),
            "refactor" => Some(Self::Refactor),
            "discovery" => Some(Self::Discovery),
            _ => None,
        }
    }

    const fn path_segment(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Gotcha => "gotcha",
            Self::SecurityNote => "security_note",
            Self::Feature => "feature",
            Self::Bugfix => "bugfix",
            Self::Change => "change",
            Self::Refactor => "refactor",
            Self::Discovery => "discovery",
        }
    }

    const fn kind(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Gotcha | Self::SecurityNote => "gotcha",
            Self::Feature | Self::Bugfix | Self::Change | Self::Refactor | Self::Discovery => {
                "fact"
            }
        }
    }
}

impl SessionJoin {
    const fn column(self) -> &'static str {
        match self {
            Self::Memory => "memory_session_id",
            Self::Sdk => "sdk_session_id",
        }
    }
}

#[derive(Debug)]
struct Plan {
    pages: Vec<PlannedPage>,
    dropped: BTreeMap<String, usize>,
    unmapped: BTreeMap<String, usize>,
    source_sha256: String,
    schema_version: i64,
    layout: ObservationLayout,
}

#[derive(Debug)]
struct Snapshot {
    _dir: TempDir,
    path: PathBuf,
    source_sha256: String,
}

pub(crate) async fn run(args: ClaudeMemArgs) -> Result<()> {
    if args.apply && args.manifest_out.is_none() {
        bail!("--apply requires --manifest-out <path>");
    }
    let source = resolve_source(args.database.as_deref())?;
    let prefix = validate_dest_prefix(&args.dest_prefix)?;
    let mapping = Mapping::load(&args.mapping)?;
    let snapshot = snapshot_database(&source)?;
    let plan = plan_snapshot(&snapshot, &mapping, &prefix)?;
    let mut manifest = Manifest::new(
        IMPORT_VERSION,
        source.to_string_lossy().into_owned(),
        plan.source_sha256.clone(),
        &plan.pages,
    );
    manifest.dropped = plan.dropped.clone();
    manifest.unmapped_projects = plan.unmapped.clone();

    if !args.apply {
        println!(
            "claude-mem schema v{} ({})",
            plan.schema_version,
            plan.layout.name()
        );
        print_plan(&plan.pages, &manifest, args.show_body);
        if let Some(path) = args.manifest_out.as_deref() {
            manifest.write(path)?;
        }
        return Ok(());
    }

    let manifest_path = args
        .manifest_out
        .as_deref()
        .ok_or_else(|| anyhow!("--apply requires --manifest-out <path>"))?;
    if !plan.unmapped.is_empty() {
        let projects: Vec<_> = plan.unmapped.keys().map(String::as_str).collect();
        let error = anyhow!(
            "the mapping file does not cover claude-mem project(s) {projects:?}; add them (or `unscoped`) and rerun"
        );
        return Err(manifest.fail(manifest_path, error));
    }
    manifest.write(manifest_path)?;
    let client = ImportClient::new(&args.server_url)?;
    if let Err(error) = client.ensure_batch_writes().await {
        return Err(manifest.fail(manifest_path, error));
    }
    let destinations: BTreeSet<_> = plan
        .pages
        .iter()
        .map(|page| page.destination.clone())
        .collect();
    for destination in &destinations {
        if let Err(error) = client
            .mcp_resolve_write_target(
                &destination.workspace,
                &destination.project,
                destination.project_id.as_deref(),
            )
            .await
        {
            return Err(manifest.fail(manifest_path, error));
        }
    }
    apply_pages_batched(&client, &plan.pages, &mut manifest, manifest_path).await?;
    print_done(&manifest);
    Ok(())
}

fn resolve_source(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_owned());
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        anyhow!("HOME is not set; pass --database instead of relying on {DEFAULT_SOURCE}")
    })?;
    Ok(PathBuf::from(home).join(".claude-mem/claude-mem.db"))
}

fn snapshot_database(source: &Path) -> Result<Snapshot> {
    let link_metadata = fs::symlink_metadata(source)
        .with_context(|| format!("inspect claude-mem database {}", source.display()))?;
    if link_metadata.file_type().is_symlink() || !link_metadata.is_file() {
        bail!("claude-mem database must be a regular file, not a symlink");
    }
    if link_metadata.len() > MAX_SOURCE_BYTES {
        bail!(
            "claude-mem database is {} bytes; maximum is {MAX_SOURCE_BYTES}",
            link_metadata.len()
        );
    }
    let identity_before = FileIdentity::from_metadata(&link_metadata);
    let source_connection = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open claude-mem database {} read-only", source.display()))?;
    source_connection
        .pragma_update(None, "query_only", true)
        .context("make claude-mem source connection query-only")?;

    let dir = tempfile::Builder::new()
        .prefix("ai-memory-claude-mem-")
        .tempdir()
        .context("create private SQLite snapshot directory")?;
    let path = dir.path().join("snapshot.db");
    let mut destination = Connection::open(&path).context("create SQLite snapshot")?;
    let backup = Backup::new(&source_connection, &mut destination)
        .context("start WAL-consistent SQLite backup")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut transient_retries = 0usize;
    loop {
        if Instant::now() >= deadline {
            bail!("claude-mem database stayed busy for the 30-second snapshot deadline");
        }
        match backup
            .step(128)
            .context("copy WAL-consistent SQLite snapshot")?
        {
            StepResult::Done => break,
            StepResult::More => transient_retries = 0,
            StepResult::Busy | StepResult::Locked => {
                transient_retries += 1;
                if transient_retries > 100 {
                    bail!("claude-mem database remained busy after 100 snapshot retries");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => bail!("claude-mem database returned an unknown backup state"),
        }
    }
    drop(backup);
    drop(destination);
    drop(source_connection);

    let after = fs::symlink_metadata(source)
        .with_context(|| format!("recheck claude-mem database {}", source.display()))?;
    if after.file_type().is_symlink() || FileIdentity::from_metadata(&after) != identity_before {
        bail!("claude-mem database changed identity while it was being snapshotted; retry");
    }
    let mut file = fs::File::open(&path).context("open SQLite snapshot for hashing")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).context("hash SQLite snapshot")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Snapshot {
        _dir: dir,
        path,
        source_sha256: format!("{:x}", hasher.finalize()),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        }
    }
}

fn plan_snapshot(snapshot: &Snapshot, mapping: &Mapping, prefix: &str) -> Result<Plan> {
    let connection = Connection::open_with_flags(
        &snapshot.path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.pragma_update(None, "query_only", true)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_SQLITE_VALUE_BYTES)
        .context("bound claude-mem SQLite cell allocation")?;
    let schema_version = inspect_schema_version(&connection)?;
    let session_columns = table_columns(&connection, "sdk_sessions")?;
    require_columns(
        "sdk_sessions",
        &session_columns,
        &["project"],
        schema_version,
    )?;
    let observation_columns = table_columns(&connection, "observations")?;
    require_columns(
        "observations",
        &observation_columns,
        &["id", "project", "type", "created_at"],
        schema_version,
    )?;
    let layout = if observation_columns.contains("title")
        && observation_columns.contains("narrative")
        && observation_columns.contains("facts")
    {
        ObservationLayout::Structured
    } else if observation_columns.contains("text") {
        ObservationLayout::LegacyText
    } else {
        bail!(
            "unsupported claude-mem schema v{schema_version}: observations matches neither the legacy text nor structured family; columns={:?}",
            sorted(&observation_columns)
        );
    };
    let summary_columns = table_columns(&connection, "session_summaries")?;
    require_columns(
        "session_summaries",
        &summary_columns,
        &["id", "project", "created_at"],
        schema_version,
    )?;
    let join = session_join(
        schema_version,
        &session_columns,
        &observation_columns,
        &summary_columns,
    )?;

    let mut pages = Vec::new();
    let mut dropped = BTreeMap::new();
    let mut unmapped = BTreeMap::new();
    let mut planned_body_bytes = 0usize;
    read_observations(
        &connection,
        layout,
        join,
        session_columns.contains("cwd"),
        mapping,
        prefix,
        &mut pages,
        &mut dropped,
        &mut unmapped,
        &mut planned_body_bytes,
    )?;
    read_summaries(
        &connection,
        session_columns.contains("cwd"),
        join,
        &summary_columns,
        mapping,
        prefix,
        &mut pages,
        &mut unmapped,
        &mut planned_body_bytes,
    )?;
    pages.sort_by(|left, right| {
        (&left.destination, &left.path).cmp(&(&right.destination, &right.path))
    });
    let mut destinations = BTreeSet::new();
    for page in &pages {
        validate_planned_path(&page.path, prefix)?;
        if !destinations.insert((page.destination.clone(), page.path.clone())) {
            bail!("two claude-mem rows map to {}", page.path);
        }
    }
    Ok(Plan {
        pages,
        dropped,
        unmapped,
        source_sha256: snapshot.source_sha256.clone(),
        schema_version,
        layout,
    })
}

fn inspect_schema_version(connection: &Connection) -> Result<i64> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_versions')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        bail!("unsupported claude-mem database: schema_versions table is missing");
    }
    connection
        .query_row("SELECT MAX(version) FROM schema_versions", [], |row| {
            row.get::<_, Option<i64>>(0)
        })?
        .ok_or_else(|| anyhow!("unsupported claude-mem database: schema_versions is empty"))
}

fn table_columns(connection: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    if columns.is_empty() {
        bail!("unsupported claude-mem database: table {table} is missing");
    }
    Ok(columns)
}

fn require_columns(
    table: &str,
    present: &HashSet<String>,
    required: &[&str],
    version: i64,
) -> Result<()> {
    let missing: Vec<_> = required
        .iter()
        .copied()
        .filter(|column| !present.contains(*column))
        .collect();
    if !missing.is_empty() {
        bail!(
            "unsupported claude-mem schema v{version}: {table} lacks required columns {missing:?}; columns={:?}",
            sorted(present)
        );
    }
    Ok(())
}

fn session_join(
    version: i64,
    sessions: &HashSet<String>,
    observations: &HashSet<String>,
    summaries: &HashSet<String>,
) -> Result<SessionJoin> {
    let all_have = |column: &str| {
        sessions.contains(column) && observations.contains(column) && summaries.contains(column)
    };
    if all_have("memory_session_id") {
        return Ok(SessionJoin::Memory);
    }
    if all_have("sdk_session_id") {
        return Ok(SessionJoin::Sdk);
    }
    bail!(
        "unsupported claude-mem schema v{version}: the session relationship must consistently use memory_session_id (v17+) or sdk_session_id (pre-v17); sdk_sessions={:?}, observations={:?}, session_summaries={:?}",
        sorted(sessions),
        sorted(observations),
        sorted(summaries)
    )
}

fn sorted(columns: &HashSet<String>) -> Vec<&str> {
    let mut sorted: Vec<_> = columns.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted
}

#[allow(clippy::too_many_arguments)]
fn read_observations(
    connection: &Connection,
    layout: ObservationLayout,
    join: SessionJoin,
    has_cwd: bool,
    mapping: &Mapping,
    prefix: &str,
    pages: &mut Vec<PlannedPage>,
    dropped: &mut BTreeMap<String, usize>,
    unmapped: &mut BTreeMap<String, usize>,
    planned_body_bytes: &mut usize,
) -> Result<()> {
    let cwd = if has_cwd { "s.cwd" } else { "NULL" };
    let fields = match layout {
        ObservationLayout::LegacyText => "o.text, NULL, NULL, NULL, NULL, NULL, NULL",
        ObservationLayout::Structured => {
            "o.text, o.title, o.subtitle, o.facts, o.narrative, o.files_read, o.files_modified"
        }
    };
    let join_column = join.column();
    let sql = format!(
        "SELECT o.id, o.project, {cwd}, o.type, o.created_at, {fields} \
         FROM observations o LEFT JOIN sdk_sessions s ON s.{join_column}=o.{join_column} \
         ORDER BY o.id LIMIT ?1",
    );
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query([i64::try_from(MAX_ROWS + 1)?])?;
    let mut count = 0usize;
    while let Some(row) = rows.next()? {
        count += 1;
        if count > MAX_ROWS {
            bail!(
                "claude-mem has more than {MAX_ROWS} observations; narrow/export the source first"
            );
        }
        let id: i64 = row.get(0)?;
        let project = bounded_metadata(row.get(1)?, "observation project", MAX_PROJECT_BYTES)?;
        let cwd = optional_bounded_metadata(row.get(2)?, "observation cwd", MAX_CWD_BYTES)?;
        let record_type = bounded_metadata(row.get(3)?, "observation type", MAX_TYPE_BYTES)?;
        if matches!(record_type.as_str(), "sensitive" | "security_alert") {
            *dropped
                .entry(format!("{record_type}_observations"))
                .or_default() += 1;
            continue;
        }
        let Some(observation_type) = ObservationType::parse(&record_type) else {
            *dropped
                .entry("unknown_type_observations".into())
                .or_default() += 1;
            continue;
        };
        let destination = match mapping.resolve(Some(&project), cwd.as_deref()) {
            Ok(destination) => destination,
            Err(key) => {
                *unmapped.entry(key).or_default() += 1;
                continue;
            }
        };
        let created_at =
            bounded_metadata(row.get(4)?, "observation created_at", MAX_TIMESTAMP_BYTES)?;
        let text: Option<String> = bounded(row.get(5)?, "observation text")?;
        let title: Option<String> = bounded(row.get(6)?, "observation title")?;
        let subtitle: Option<String> = bounded(row.get(7)?, "observation subtitle")?;
        let facts: Option<String> = bounded(row.get(8)?, "observation facts")?;
        let narrative: Option<String> = bounded(row.get(9)?, "observation narrative")?;
        let files_read: Option<String> = bounded(row.get(10)?, "observation files_read")?;
        let files_modified: Option<String> = bounded(row.get(11)?, "observation files_modified")?;
        let source_id = id.to_string();
        let body = observation_body(
            &created_at,
            text.as_deref(),
            subtitle.as_deref(),
            facts.as_deref(),
            narrative.as_deref(),
            files_read.as_deref(),
            files_modified.as_deref(),
        );
        let title = title
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| one_line(text.as_deref().unwrap_or(&record_type), 80));
        let planned = page(
            "claude_mem_observation",
            &source_id,
            destination,
            format!("{prefix}/{}/{id}.md", observation_type.path_segment()),
            title,
            body,
            observation_type.kind(),
            &["claude-mem-import", "observation"],
        );
        push_planned_page(pages, planned, planned_body_bytes, MAX_PLANNED_BODY_BYTES)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn read_summaries(
    connection: &Connection,
    has_cwd: bool,
    join: SessionJoin,
    columns: &HashSet<String>,
    mapping: &Mapping,
    prefix: &str,
    pages: &mut Vec<PlannedPage>,
    unmapped: &mut BTreeMap<String, usize>,
    planned_body_bytes: &mut usize,
) -> Result<()> {
    let cwd = if has_cwd { "s.cwd" } else { "NULL" };
    let optional = |name: &str| {
        if columns.contains(name) {
            format!("ss.{name}")
        } else {
            "NULL".into()
        }
    };
    let join_column = join.column();
    let sql = format!(
        "SELECT ss.id, ss.project, {cwd}, ss.created_at, {}, {}, {}, {}, {}, {}, {}, {} \
         FROM session_summaries ss LEFT JOIN sdk_sessions s ON s.{join_column}=ss.{join_column} \
         ORDER BY ss.id LIMIT ?1",
        optional("request"),
        optional("investigated"),
        optional("learned"),
        optional("completed"),
        optional("next_steps"),
        optional("files_read"),
        optional("files_edited"),
        optional("notes")
    );
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query([i64::try_from(MAX_ROWS + 1)?])?;
    let mut count = 0usize;
    while let Some(row) = rows.next()? {
        count += 1;
        if count > MAX_ROWS {
            bail!("claude-mem has more than {MAX_ROWS} summaries; narrow/export the source first");
        }
        let id: i64 = row.get(0)?;
        let project = bounded_metadata(row.get(1)?, "summary project", MAX_PROJECT_BYTES)?;
        let cwd = optional_bounded_metadata(row.get(2)?, "summary cwd", MAX_CWD_BYTES)?;
        let destination = match mapping.resolve(Some(&project), cwd.as_deref()) {
            Ok(destination) => destination,
            Err(key) => {
                *unmapped.entry(key).or_default() += 1;
                continue;
            }
        };
        let created_at = bounded_metadata(row.get(3)?, "summary created_at", MAX_TIMESTAMP_BYTES)?;
        let labels = [
            "Request",
            "Investigated",
            "Learned",
            "Completed",
            "Next steps",
            "Files read",
            "Files edited",
            "Notes",
        ];
        let mut sections = Vec::new();
        for (offset, label) in labels.iter().enumerate() {
            let value: Option<String> = bounded(row.get(4 + offset)?, "summary field")?;
            if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                sections.push(format!("## {label}\n\n{}", value.trim()));
            }
        }
        let body = format!(
            "Imported at source timestamp: `{created_at}`\n\n{}",
            sections.join("\n\n")
        );
        let source_id = id.to_string();
        let planned = page(
            "claude_mem_summary",
            &source_id,
            destination,
            format!("{prefix}/session-summary/{id}.md"),
            format!("Session summary {id}"),
            body,
            "session-summary",
            &["claude-mem-import", "session-summary"],
        );
        push_planned_page(pages, planned, planned_body_bytes, MAX_PLANNED_BODY_BYTES)?;
    }
    Ok(())
}

fn bounded(value: Option<String>, field: &str) -> Result<Option<String>> {
    if value
        .as_ref()
        .is_some_and(|value| value.len() > MAX_FIELD_BYTES)
    {
        bail!("{field} exceeds the {MAX_FIELD_BYTES}-byte limit");
    }
    Ok(value)
}

fn push_planned_page(
    pages: &mut Vec<PlannedPage>,
    page: PlannedPage,
    total: &mut usize,
    max: usize,
) -> Result<()> {
    let bytes = page
        .versions
        .iter()
        .try_fold(0usize, |sum, version| sum.checked_add(version.body.len()))
        .ok_or_else(|| anyhow!("planned claude-mem body size overflowed"))?;
    let next = total
        .checked_add(bytes)
        .ok_or_else(|| anyhow!("planned claude-mem body size overflowed"))?;
    if next > max {
        bail!(
            "planned claude-mem page bodies exceed {max} bytes; export or narrow the source before importing"
        );
    }
    *total = next;
    pages.push(page);
    Ok(())
}

fn bounded_metadata(value: String, field: &str, max: usize) -> Result<String> {
    if value.len() > max {
        bail!("{field} exceeds the {max}-byte limit");
    }
    if value.chars().any(char::is_control) {
        bail!("{field} contains control characters");
    }
    Ok(ai_memory_client::sanitize_external_text(&value))
}

fn optional_bounded_metadata(
    value: Option<String>,
    field: &str,
    max: usize,
) -> Result<Option<String>> {
    value
        .map(|value| bounded_metadata(value, field, max))
        .transpose()
}

#[allow(clippy::too_many_arguments)]
fn observation_body(
    created_at: &str,
    text: Option<&str>,
    subtitle: Option<&str>,
    facts: Option<&str>,
    narrative: Option<&str>,
    files_read: Option<&str>,
    files_modified: Option<&str>,
) -> String {
    let mut body = format!("Imported at source timestamp: `{created_at}`");
    for (heading, value) in [
        ("Observation", text),
        ("Subtitle", subtitle),
        ("Facts", facts),
        ("Narrative", narrative),
        ("Files read", files_read),
        ("Files modified", files_modified),
    ] {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            body.push_str(&format!("\n\n## {heading}\n\n{}", value.trim()));
        }
    }
    body
}

#[allow(clippy::too_many_arguments)]
fn page(
    record: &'static str,
    source_id: &str,
    destination: Destination,
    path: String,
    title: String,
    body: String,
    kind: &str,
    tags: &[&str],
) -> PlannedPage {
    let key = Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("https://github.com/akitaonrails/ai-memory/claude-mem/{record}/{source_id}")
            .as_bytes(),
    )
    .to_string();
    let sanitized = ai_memory_client::sanitize_external_text(&body);
    let footer =
        format!("*Imported from claude-mem ({record} `{source_id}`). Import key: `{key}`.*");
    PlannedPage {
        record,
        destination,
        path,
        versions: vec![PlannedVersion {
            key,
            source_id: source_id.to_owned(),
            version: None,
            title: one_line(&ai_memory_client::sanitize_external_text(&title), 80),
            body: bounded_page_body(&sanitized, &footer),
            kind: kind.to_owned(),
            tier: "episodic",
            tags: tags_from(tags.iter().copied()),
            expires_at: None,
        }],
    }
}

fn bounded_page_body(body: &str, footer: &str) -> String {
    const MARKER: &str = "\n\n[content truncated by ai-memory-importer]\n";
    let suffix = format!("\n\n---\n{footer}\n");
    let body = body.trim_end();
    if body.chars().count() + suffix.chars().count() <= READ_PAGE_MAX_CHARS {
        return format!("{body}{suffix}");
    }
    let keep = READ_PAGE_MAX_CHARS
        .saturating_sub(MARKER.chars().count())
        .saturating_sub(suffix.chars().count());
    let mut bounded: String = body.chars().take(keep).collect();
    bounded.push_str(MARKER);
    bounded.push_str(&suffix);
    bounded
}

#[cfg(test)]
mod e2e;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentmemory::mock::MockServer;

    fn mapping() -> Mapping {
        Mapping {
            projects: BTreeMap::from([(
                "api".into(),
                Destination {
                    workspace: "acme".into(),
                    project: "api".into(),
                    project_id: None,
                },
            )]),
            cwds: BTreeMap::new(),
            unscoped: None,
        }
    }

    fn fixture(structured: bool) -> (TempDir, PathBuf) {
        fixture_with_join(structured, "memory_session_id")
    }

    fn fixture_with_join(structured: bool, join: &str) -> (TempDir, PathBuf) {
        assert!(matches!(join, "memory_session_id" | "sdk_session_id"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude-mem.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TABLE schema_versions(id INTEGER PRIMARY KEY, version INTEGER UNIQUE NOT NULL, applied_at TEXT NOT NULL);\
                 INSERT INTO schema_versions(version,applied_at) VALUES(4,'now');\
                 CREATE TABLE sdk_sessions({join} TEXT UNIQUE, project TEXT NOT NULL, cwd TEXT);\
                 INSERT INTO sdk_sessions VALUES('m1','api','/repo/api');\
                 CREATE TABLE session_summaries(id INTEGER PRIMARY KEY, {join} TEXT, project TEXT NOT NULL, request TEXT, learned TEXT, created_at TEXT NOT NULL);\
                 INSERT INTO session_summaries VALUES(7,'m1','api','fix it','lesson','2026-01-01');"
            ))
            .unwrap();
        if structured {
            connection.execute_batch(&format!(
                "CREATE TABLE observations(id INTEGER PRIMARY KEY, {join} TEXT, project TEXT NOT NULL, text TEXT, type TEXT NOT NULL, title TEXT, subtitle TEXT, facts TEXT, narrative TEXT, files_read TEXT, files_modified TEXT, created_at TEXT NOT NULL);\
                 INSERT INTO observations VALUES(1,'m1','api',NULL,'decision','Use sqlite',NULL,'[]','Chosen','[]','[]','2026-01-01');\
                 INSERT INTO observations VALUES(2,'m1','api','secret','sensitive','No',NULL,NULL,NULL,NULL,NULL,'2026-01-01');\
                 INSERT INTO observations VALUES(3,'m1','api',NULL,'security_note','Check permissions',NULL,'[]','Least privilege','[]','[]','2026-01-01');\
                 INSERT INTO observations VALUES(4,'m1','api','future secret','future_type','Unknown',NULL,NULL,NULL,NULL,NULL,'2026-01-01');"
            )).unwrap();
        } else {
            connection.execute_batch(&format!(
                "CREATE TABLE observations(id INTEGER PRIMARY KEY, {join} TEXT, project TEXT NOT NULL, text TEXT, type TEXT NOT NULL, created_at TEXT NOT NULL);\
                 INSERT INTO observations VALUES(1,'m1','api','Use sqlite','decision','2026-01-01');"
            )).unwrap();
        }
        drop(connection);
        (dir, path)
    }

    #[test]
    fn plans_known_legacy_and_structured_families_and_skips_sensitive_rows() {
        for (structured, layout) in [
            (false, ObservationLayout::LegacyText),
            (true, ObservationLayout::Structured),
        ] {
            let (_dir, path) = fixture(structured);
            let snapshot = snapshot_database(&path).unwrap();
            let plan = plan_snapshot(&snapshot, &mapping(), DEFAULT_DEST_PREFIX).unwrap();
            assert_eq!(plan.layout, layout);
            assert_eq!(plan.pages.len(), if structured { 3 } else { 2 });
            assert!(
                plan.pages
                    .iter()
                    .all(|page| page.path.starts_with(DEFAULT_DEST_PREFIX))
            );
            assert_eq!(
                plan.dropped
                    .get("sensitive_observations")
                    .copied()
                    .unwrap_or(0),
                usize::from(structured)
            );
            assert_eq!(
                plan.dropped
                    .get("unknown_type_observations")
                    .copied()
                    .unwrap_or(0),
                usize::from(structured)
            );
            let manifest = Manifest::new(
                IMPORT_VERSION,
                path.display().to_string(),
                plan.source_sha256,
                &plan.pages,
            );
            let json = serde_json::to_string(&manifest).unwrap();
            assert!(!json.contains("secret"));
            assert!(
                plan.pages
                    .iter()
                    .any(|page| page.path.ends_with("decision/1.md"))
            );
            assert!(
                plan.pages
                    .iter()
                    .any(|page| page.path.ends_with("session-summary/7.md"))
            );
            if structured {
                let note = plan
                    .pages
                    .iter()
                    .find(|page| page.path.ends_with("security_note/3.md"))
                    .unwrap();
                assert_eq!(note.versions[0].kind, "gotcha");
                assert!(note.versions[0].body.contains("Least privilege"));
            }
        }
    }

    #[test]
    fn supports_pre_v17_sdk_session_relationship_and_rejects_mixed_relationships() {
        let (_dir, path) = fixture_with_join(false, "sdk_session_id");
        let snapshot = snapshot_database(&path).unwrap();
        let plan = plan_snapshot(&snapshot, &mapping(), DEFAULT_DEST_PREFIX).unwrap();
        assert_eq!(plan.pages.len(), 2);
        assert!(
            plan.pages
                .iter()
                .any(|page| page.path.ends_with("decision/1.md"))
        );

        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE observations RENAME COLUMN sdk_session_id TO memory_session_id",
            )
            .unwrap();
        drop(connection);
        let snapshot = snapshot_database(&path).unwrap();
        let error = plan_snapshot(&snapshot, &mapping(), DEFAULT_DEST_PREFIX).unwrap_err();
        assert!(error.to_string().contains("consistently use"), "{error:#}");
    }

    #[test]
    fn observation_type_mapping_is_closed_and_matches_the_import_contract() {
        for (source, path, kind) in [
            ("decision", "decision", "decision"),
            ("gotcha", "gotcha", "gotcha"),
            ("security_note", "security_note", "gotcha"),
            ("feature", "feature", "fact"),
            ("bugfix", "bugfix", "fact"),
            ("change", "change", "fact"),
            ("refactor", "refactor", "fact"),
            ("discovery", "discovery", "fact"),
        ] {
            let mapped = ObservationType::parse(source).unwrap();
            assert_eq!(mapped.path_segment(), path);
            assert_eq!(mapped.kind(), kind);
        }
        assert_eq!(ObservationType::parse("tool_output"), None);
    }

    #[test]
    fn rejects_unknown_schema_and_source_symlinks() {
        let (_dir, path) = fixture(false);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("ALTER TABLE observations RENAME TO unknown_observations")
            .unwrap();
        drop(connection);
        let snapshot = snapshot_database(&path).unwrap();
        assert!(
            plan_snapshot(&snapshot, &mapping(), DEFAULT_DEST_PREFIX)
                .unwrap_err()
                .to_string()
                .contains("observations is missing")
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&path, snapshot._dir.path().join("link.db")).unwrap();
            assert!(
                snapshot_database(&snapshot._dir.path().join("link.db"))
                    .unwrap_err()
                    .to_string()
                    .contains("not a symlink")
            );
        }
    }

    #[test]
    fn cwd_mapping_wins_and_unmapped_rows_are_counted_without_bodies() {
        let (_dir, path) = fixture(false);
        let mut mapping = mapping();
        mapping.cwds.insert(
            "/repo/api".into(),
            Destination {
                workspace: "other".into(),
                project: "checkout".into(),
                project_id: None,
            },
        );
        let snapshot = snapshot_database(&path).unwrap();
        let plan = plan_snapshot(&snapshot, &mapping, DEFAULT_DEST_PREFIX).unwrap();
        assert!(
            plan.pages
                .iter()
                .all(|page| page.destination.project == "checkout")
        );

        let empty = Mapping {
            projects: BTreeMap::new(),
            cwds: BTreeMap::new(),
            unscoped: None,
        };
        let plan = plan_snapshot(&snapshot, &empty, DEFAULT_DEST_PREFIX).unwrap();
        assert!(plan.pages.is_empty());
        assert_eq!(plan.unmapped.get("api"), Some(&2));
    }

    #[tokio::test]
    async fn apply_uses_atomic_batches_stable_project_id_and_exact_body_resume() {
        let (_dir, database) = fixture(true);
        let connection = Connection::open(&database).unwrap();
        connection
            .execute(
                "UPDATE observations SET narrative=?1 WHERE id=1",
                ["x".repeat(100_000)],
            )
            .unwrap();
        drop(connection);
        let files = tempfile::tempdir().unwrap();
        let mapping = files.path().join("mapping.json");
        fs::write(
            &mapping,
            r#"{"projects":{"api":{"workspace":"acme","project_id":"019a0000-0000-7000-8000-000000000001"}}}"#,
        )
        .unwrap();
        let manifest = files.path().join("manifest.json");
        let project_id = "019a0000-0000-7000-8000-000000000001";
        let server = MockServer::start(&[("acme", project_id)]);
        let args = || ClaudeMemArgs {
            database: Some(database.clone()),
            mapping: mapping.clone(),
            dest_prefix: DEFAULT_DEST_PREFIX.into(),
            server_url: server.url.clone(),
            apply: true,
            manifest_out: Some(manifest.clone()),
            show_body: false,
        };

        run(args()).await.unwrap();
        assert_eq!(server.state().stored_versions(), 3);
        assert_eq!(server.state().batch_calls, 1);
        let decision =
            server
                .state()
                .chain("acme", project_id, "imported/claude-mem/decision/1.md");
        let body = &decision[0].body;
        assert!(body.contains("[content truncated by ai-memory-importer]"));
        assert!(body.chars().count() <= READ_PAGE_MAX_CHARS);
        run(args()).await.unwrap();
        assert_eq!(server.state().stored_versions(), 3);
        assert_eq!(server.state().batch_calls, 4);
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(manifest).unwrap()).unwrap();
        assert_eq!(manifest["written_versions"], 0);
        assert_eq!(manifest["present_versions"], 3);
        assert!(
            manifest["pages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|page| page["project"] == project_id)
        );
    }

    #[tokio::test]
    async fn all_destinations_are_preflighted_before_the_first_batch() {
        let (_dir, database) = fixture(false);
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "INSERT INTO sdk_sessions VALUES('m2','other','/repo/other');\
                 INSERT INTO observations VALUES(2,'m2','other','Another decision','decision','2026-01-01');",
            )
            .unwrap();
        drop(connection);
        let files = tempfile::tempdir().unwrap();
        let mapping = files.path().join("mapping.json");
        fs::write(
            &mapping,
            r#"{"projects":{"api":{"workspace":"acme","project":"api"},"other":{"workspace":"acme","project_id":"019a0000-0000-7000-8000-000000000099"}}}"#,
        )
        .unwrap();
        let server = MockServer::start(&[("acme", "api")]);
        let error = run(ClaudeMemArgs {
            database: Some(database),
            mapping,
            dest_prefix: DEFAULT_DEST_PREFIX.into(),
            server_url: server.url.clone(),
            apply: true,
            manifest_out: Some(files.path().join("manifest.json")),
            show_body: false,
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("scope not found"), "{error:#}");
        assert_eq!(server.state().batch_calls, 0);
        assert_eq!(server.state().stored_versions(), 0);
    }

    #[test]
    fn mapping_project_ids_are_bounded_validated_and_canonicalized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mapping.json");
        fs::write(
            &path,
            r#"{"projects":{"api":{"workspace":"acme","project_id":"019A0000-0000-7000-8000-000000000001"}}}"#,
        )
        .unwrap();
        let mapping = Mapping::load(&path).unwrap();
        assert_eq!(
            mapping.projects["api"].project_id.as_deref(),
            Some("019a0000-0000-7000-8000-000000000001")
        );
        for bad in ["not-a-uuid".to_owned(), "x".repeat(1_000)] {
            fs::write(
                &path,
                format!(r#"{{"projects":{{"api":{{"workspace":"acme","project_id":"{bad}"}}}}}}"#),
            )
            .unwrap();
            assert!(Mapping::load(&path).is_err());
        }
    }

    #[test]
    fn source_metadata_is_bounded_sanitized_and_control_free() {
        assert!(bounded_metadata("x".repeat(513), "project", 512).is_err());
        assert!(bounded_metadata("bad\nproject".into(), "project", 512).is_err());
        let marker = format!("{}{}", "gh", "p_abcdefghijklmnopqrstuvwxyz1234567890");
        let sanitized = bounded_metadata(format!("token {marker}"), "project", 512).unwrap();
        assert!(!sanitized.contains(&marker));
    }

    #[test]
    fn sqlite_cell_and_aggregate_plan_limits_fail_before_unbounded_planning() {
        let (_dir, database) = fixture(false);
        let connection = Connection::open(&database).unwrap();
        connection
            .execute(
                "UPDATE observations SET text=?1 WHERE id=1",
                ["x".repeat(MAX_SQLITE_VALUE_BYTES as usize + 1)],
            )
            .unwrap();
        drop(connection);
        let snapshot = snapshot_database(&database).unwrap();
        assert!(plan_snapshot(&snapshot, &mapping(), DEFAULT_DEST_PREFIX).is_err());

        let planned = page(
            "claude_mem_observation",
            "1",
            Destination {
                workspace: "acme".into(),
                project: "api".into(),
                project_id: None,
            },
            "imported/claude-mem/decision/1.md".into(),
            "Decision".into(),
            "body".into(),
            "decision",
            &["claude-mem-import"],
        );
        let mut pages = Vec::new();
        let mut total = 0;
        let error = push_planned_page(&mut pages, planned, &mut total, 1).unwrap_err();
        assert!(error.to_string().contains("export or narrow"), "{error:#}");
        assert!(pages.is_empty());
    }
}
