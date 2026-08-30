//! Pure, IO-free capture policy evaluation for native hooks and server defense.

use ai_memory_core::AgentKind;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};

/// Maximum number of `ignore_paths` entries (entries, not bytes).
pub const MAX_IGNORE_PATTERNS: usize = 128;
/// Maximum Unicode scalar characters in one pattern.
pub const MAX_IGNORE_PATTERN_CHARS: usize = 1_024;
/// Marker readers must enforce this maximum byte read before parsing.
pub const MAX_MARKER_BYTES: usize = 64 * 1024;
/// Maximum Unicode scalar characters accepted in an extracted candidate path.
pub const MAX_CANDIDATE_PATH_CHARS: usize = 4_096;
/// Maximum direct path candidates accepted from one recognized file tool call.
pub const MAX_CAPTURE_CANDIDATES: usize = 32;
/// Maximum aggregate pattern-by-candidate scalar comparisons per inspection.
pub const MAX_MATCH_WORK: usize = 1_000_000;
const MAX_CALL_ID_CHARS: usize = 128;
const CAPTURE_PROTOCOL_VERSION: u8 = 1;

/// Minimal decoded `[capture]` marker configuration.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CaptureConfig {
    /// Whole-path glob patterns to exclude.
    #[serde(default)]
    pub ignore_paths: Vec<String>,
}

/// Whether a repository is captured unless excluded, or only when it opts in.
///
/// This is the failure-mode switch requested in #446. Under [`Self::Denylist`]
/// — the historical behaviour and still the default — a repository with no
/// marker is captured, so forgetting a marker leaks. Under
/// [`Self::Allowlist`] the same omission captures nothing.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureMode {
    /// Absence of a marker means capture normally.
    #[default]
    Denylist,
    /// Absence of a marker means capture nothing at all.
    Allowlist,
}

/// Whether this repository may emit *any* lifecycle event, decided before the
/// per-event policy in [`CapturePolicy::inspect`] and before anything is
/// spooled.
///
/// Deliberately independent of the event kind. `inspect` is reached only for
/// tool events (`is_tool_event` in the CLI hook), so a gate expressed through
/// [`CaptureDisposition`] alone would leave `UserPromptSubmit`,
/// `SessionStart`/`SessionEnd`, and `Stop` bodies flowing while reporting the
/// repository as opted out — the precise false guarantee #446 is about. Prompt
/// text is the field that issue cares about most, so this must gate every
/// event or it gates nothing that matters.
#[must_use]
pub const fn repository_admits_capture(mode: CaptureMode, marker_present: bool) -> bool {
    match mode {
        CaptureMode::Denylist => true,
        CaptureMode::Allowlist => marker_present,
    }
}

/// Typed result of marker discovery and parsing, supplied by the IO-owning caller.
pub enum CaptureSource<'a> {
    /// No nearest marker exists.
    Absent,
    /// A complete marker parsed into the strict capture configuration.
    Parsed(&'a CaptureConfig),
    /// Discovery, bounded read, TOML parsing, or type validation failed.
    Invalid,
}

/// Local capture action.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureDisposition {
    /// Preserve the original event.
    Keep,
    /// Do not spool, queue, or transmit the event.
    Drop,
    /// Replace the event body with the strict metadata allowlist.
    MetadataOnly,
}

/// Resolution state of marker configuration.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyState {
    /// No marker or an explicitly empty list.
    Inactive,
    /// A complete valid pattern set is active.
    Active,
    /// Marker/configuration failed strict validation.
    Invalid,
}

/// Result of direct, schema-specific tool extraction.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ExtractionState {
    /// Tool does not require path extraction.
    NotApplicable,
    /// Direct fields were extracted successfully.
    Extracted,
    /// A recognized file tool had absent, malformed, blank, or unusable fields.
    MissingOrMalformed,
    /// The agent payload did not use an explicitly supported adapter schema.
    #[default]
    UnsupportedSchema,
}

/// Canonical category used in the reserved protocol.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ToolFamily {
    /// Recognized direct file operation.
    File,
    /// Recognized search/list operation.
    SearchList,
    /// Explicitly known non-file operation.
    NonFile,
    /// Unknown or unsupported tool.
    #[default]
    Unknown,
}

/// Safe, closed-schema tool metadata suitable for an observation body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolObservationMetadata {
    pub(crate) tool_family: ToolFamily,
    pub(crate) tool_call_id: Option<String>,
}

/// Safe outcome class for a completed tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolOutcome {
    Success,
    Error,
    Unknown,
}

impl ToolOutcome {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Error => "error",
            Self::Unknown => "unknown",
        }
    }
}

/// Extracts only the closed, fixture-backed metadata schemas used for tool
/// observations. It intentionally does not reuse broad envelope fallbacks.
pub(crate) fn tool_observation_metadata(
    agent: AgentKind,
    raw: &Value,
    require_input: bool,
) -> Option<ToolObservationMetadata> {
    let object = raw.as_object()?;
    let (name, id) = match agent {
        // ZCode tool payloads carry Claude Code's snake_case aliases
        // (`tool_name`, `tool_use_id`, `tool_input`) alongside the native
        // camelCase — all captured live (engine v0.16.5, #512).
        AgentKind::ClaudeCode | AgentKind::CommandCode | AgentKind::Zcode => (
            object.get("tool_name")?.as_str()?,
            object.get("tool_use_id").and_then(Value::as_str),
        ),
        AgentKind::OpenCode => (
            object.get("tool")?.as_str()?,
            object.get("callID").and_then(Value::as_str),
        ),
        AgentKind::Pi => (
            object.get("tool")?.as_str()?,
            object.get("callID").and_then(Value::as_str),
        ),
        // Kiro v2/v3 tool hooks use `tool_name` + `tool_input`. Unknown payload
        // shapes fail safe to metadata-only under an active policy.
        AgentKind::KiroCli => (object.get("tool_name")?.as_str()?, None),
        AgentKind::AntigravityCli => (object.get("toolCall")?.get("name")?.as_str()?, None),
        AgentKind::Hermes => (
            object.get("tool_name")?.as_str()?,
            object
                .get("extra")
                .and_then(Value::as_object)
                .and_then(|extra| extra.get("tool_call_id"))
                .and_then(Value::as_str),
        ),
        // Pool (Poolside Agent CLI) tool hooks use snake_case `tool_name` +
        // `tool_input` (hooks api 1.0, verified against Poolside CLI v1.0.16);
        // no tool-call id is documented. Unknown payload shapes fail safe to
        // metadata-only under an active policy.
        AgentKind::Pool => (object.get("tool_name")?.as_str()?, None),
        _ => return None,
    };
    // PreToolUse needs a proven input shape. PostToolUse deliberately does
    // not: established adapters commonly omit inputs from their response.
    let has_args = !require_input
        || match agent {
            AgentKind::AntigravityCli => object.get("toolCall")?.get("args").is_some(),
            _ => object
                .get(
                    if matches!(
                        agent,
                        AgentKind::ClaudeCode
                            | AgentKind::CommandCode
                            | AgentKind::Hermes
                            | AgentKind::KiroCli
                            | AgentKind::Pool
                            | AgentKind::Zcode
                    ) {
                        "tool_input"
                    } else {
                        "args"
                    },
                )
                .is_some(),
        };
    has_args.then(|| ToolObservationMetadata {
        tool_family: family(name),
        tool_call_id: id.filter(|id| valid_call_id(id)).map(str::to_owned),
    })
}

/// Extracts an outcome only where the adapter protocol proves its meaning.
pub(crate) fn tool_observation_outcome(agent: AgentKind, raw: &Value) -> ToolOutcome {
    match agent {
        AgentKind::Pi => match raw.get("isError").and_then(Value::as_bool) {
            Some(true) => ToolOutcome::Error,
            Some(false) => ToolOutcome::Success,
            None => ToolOutcome::Unknown,
        },
        AgentKind::KiroCli => match raw
            .get("tool_response")
            .and_then(|response| response.get("success"))
            .and_then(Value::as_bool)
        {
            Some(true) => ToolOutcome::Success,
            Some(false) => ToolOutcome::Error,
            None => ToolOutcome::Unknown,
        },
        AgentKind::AntigravityCli
            if raw
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|error| !error.is_empty()) =>
        {
            ToolOutcome::Error
        }
        // ZCode fires `PostToolUseFailure` instead of `PostToolUse` when the
        // tool throws; that payload carries `error` (string) + `error_details`
        // (live-captured, #512). `exitCode` is deliberately not mapped to
        // Success: the same `tool_response` object carries `timedOut` /
        // `interrupted`, so exit code alone does not prove success.
        AgentKind::Zcode
            if raw
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|error| !error.is_empty()) =>
        {
            ToolOutcome::Error
        }
        _ => ToolOutcome::Unknown,
    }
}

/// Strict, bounded reserved protocol body.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CaptureProtocol {
    version: u8,
    disposition: CaptureDisposition,
    policy_state: PolicyState,
    tool_family: ToolFamily,
    path_count: u16,
    extraction_state: ExtractionState,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProtocol {
    version: u8,
    disposition: CaptureDisposition,
    policy_state: PolicyState,
    tool_family: ToolFamily,
    path_count: u16,
    extraction_state: ExtractionState,
}

impl<'de> Deserialize<'de> for CaptureProtocol {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = WireProtocol::deserialize(deserializer)?;
        if wire.version != CAPTURE_PROTOCOL_VERSION {
            return Err(serde::de::Error::custom(
                "unsupported capture protocol version",
            ));
        }
        Ok(Self::new(
            wire.disposition,
            wire.policy_state,
            wire.tool_family,
            wire.path_count,
            wire.extraction_state,
        ))
    }
}

impl CaptureProtocol {
    fn new(
        disposition: CaptureDisposition,
        policy_state: PolicyState,
        tool_family: ToolFamily,
        path_count: u16,
        extraction_state: ExtractionState,
    ) -> Self {
        Self {
            version: CAPTURE_PROTOCOL_VERSION,
            disposition,
            policy_state,
            tool_family,
            path_count,
            extraction_state,
        }
    }
    /// Parses only the current protocol version and bounded field set.
    #[must_use]
    pub fn parse(value: &Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }
    /// Fixed wire protocol version.
    #[must_use]
    pub const fn version(&self) -> u8 {
        self.version
    }
    /// Chosen local action.
    #[must_use]
    pub const fn disposition(&self) -> CaptureDisposition {
        self.disposition
    }
    /// Resolved marker state.
    #[must_use]
    pub const fn policy_state(&self) -> PolicyState {
        self.policy_state
    }
    /// Canonical tool family.
    #[must_use]
    pub const fn tool_family(&self) -> ToolFamily {
        self.tool_family
    }
    /// Number of direct candidates, capped by the fixed `u16` wire type.
    #[must_use]
    pub const fn path_count(&self) -> u16 {
        self.path_count
    }
    /// Direct extraction result.
    #[must_use]
    pub const fn extraction_state(&self) -> ExtractionState {
        self.extraction_state
    }
}

/// Compiled policy plus its state. Construct with [`CapturePolicy::resolve`].
#[derive(Clone, Debug)]
pub struct CapturePolicy {
    state: PolicyState,
    patterns: Vec<CompiledPattern>,
}

#[derive(Clone, Debug)]
struct CompiledPattern {
    path: String,
    flavor: Flavor,
    directory_base: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavor {
    Posix,
    Windows,
}

/// Safe inspection result; it never retains raw arguments, paths, or arbitrary tool names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureDecision {
    protocol: CaptureProtocol,
    identity: CanonicalTool,
    call_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum CanonicalTool {
    File,
    SearchList,
    NonFile,
    Unknown,
}

impl CaptureDecision {
    /// Protocol clients send as `_ai_memory_capture` when the policy is active/invalid.
    #[must_use]
    pub const fn protocol(&self) -> &CaptureProtocol {
        &self.protocol
    }
}

impl CapturePolicy {
    /// Resolves a marker config atomically. Empty parsed configuration is inactive.
    /// `home_dir` is required only when at least one pattern starts with `~/`.
    #[must_use]
    pub fn resolve(source: CaptureSource<'_>, marker_dir: &str, home_dir: Option<&str>) -> Self {
        let config = match source {
            CaptureSource::Absent => return Self::inactive(),
            CaptureSource::Invalid => {
                return Self {
                    state: PolicyState::Invalid,
                    patterns: Vec::new(),
                };
            }
            CaptureSource::Parsed(config) => config,
        };
        if config.ignore_paths.is_empty() {
            return Self::inactive();
        }
        let compiled = compile(config, marker_dir, home_dir);
        match compiled {
            Ok(patterns) => Self {
                state: PolicyState::Active,
                patterns,
            },
            Err(()) => Self {
                state: PolicyState::Invalid,
                patterns: Vec::new(),
            },
        }
    }
    fn inactive() -> Self {
        Self {
            state: PolicyState::Inactive,
            patterns: Vec::new(),
        }
    }
    /// Evaluates every policy state using only direct fixture-backed schemas.
    #[must_use]
    pub fn inspect(&self, agent: AgentKind, raw: &Value, cwd: &str) -> CaptureDecision {
        let extracted = extract(agent, raw);
        let (disposition, extraction) = match self.state {
            PolicyState::Inactive => (CaptureDisposition::Keep, extracted.state),
            PolicyState::Invalid if extracted.family == ToolFamily::File => {
                (CaptureDisposition::MetadataOnly, extracted.state)
            }
            PolicyState::Invalid => (CaptureDisposition::Keep, extracted.state),
            PolicyState::Active => match extracted.family {
                ToolFamily::SearchList => {
                    (CaptureDisposition::Drop, ExtractionState::NotApplicable)
                }
                ToolFamily::File => match extracted.paths.as_ref() {
                    None => (
                        CaptureDisposition::MetadataOnly,
                        ExtractionState::MissingOrMalformed,
                    ),
                    Some(paths)
                        if paths
                            .iter()
                            .any(|path| normalize_candidate(path, cwd).is_none()) =>
                    {
                        (
                            CaptureDisposition::MetadataOnly,
                            ExtractionState::MissingOrMalformed,
                        )
                    }
                    Some(paths) => match self.match_paths(paths, cwd) {
                        Ok(true) => (CaptureDisposition::Drop, ExtractionState::Extracted),
                        Ok(false) => (CaptureDisposition::Keep, ExtractionState::Extracted),
                        Err(()) => (
                            CaptureDisposition::MetadataOnly,
                            ExtractionState::MissingOrMalformed,
                        ),
                    },
                },
                _ => (CaptureDisposition::Keep, extracted.state),
            },
        };
        CaptureDecision {
            protocol: CaptureProtocol::new(
                disposition,
                self.state,
                extracted.family,
                extracted
                    .paths
                    .as_ref()
                    .map_or(0, |p| p.len().min(u16::MAX as usize) as u16),
                extraction,
            ),
            identity: canonical(extracted.family),
            call_id: extracted.call_id,
        }
    }
    fn match_paths(&self, paths: &[String], cwd: &str) -> Result<bool, ()> {
        let candidates: Option<Vec<_>> = paths
            .iter()
            .map(|path| normalize_candidate(path, cwd))
            .collect();
        let candidates = candidates.ok_or(())?;
        let mut work = 0_usize;
        for candidate in candidates {
            for pattern in self
                .patterns
                .iter()
                .filter(|pattern| pattern.flavor == candidate.flavor)
            {
                work = work
                    .checked_add(
                        pattern
                            .path
                            .chars()
                            .count()
                            .saturating_mul(candidate.path.chars().count()),
                    )
                    .ok_or(())?;
                if work > MAX_MATCH_WORK {
                    return Err(());
                }
                if glob_match(
                    &pattern.path,
                    &candidate.path,
                    pattern.directory_base.as_deref(),
                    pattern.flavor == Flavor::Windows,
                ) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

/// Constructs a metadata-only replacement from scalar allowlisted envelope values and a decision.
#[must_use]
pub fn metadata_only_body(
    session_id: Option<&str>,
    cwd: Option<&str>,
    decision: &CaptureDecision,
) -> Value {
    let mut body = Map::new();
    if let Some(value) = session_id {
        body.insert("session_id".into(), Value::String(value.into()));
    }
    if let Some(value) = cwd {
        body.insert("cwd".into(), Value::String(value.into()));
    }
    body.insert("tool_family".into(), json!(decision.protocol.tool_family()));
    body.insert("tool_name".into(), json!(decision.identity));
    if let Some(value) = &decision.call_id {
        body.insert("tool_call_id".into(), Value::String(value.clone()));
    }
    body.insert("_ai_memory_capture".into(), json!(decision.protocol));
    Value::Object(body)
}

#[derive(Default)]
struct Extracted {
    family: ToolFamily,
    paths: Option<Vec<String>>,
    call_id: Option<String>,
    state: ExtractionState,
}

fn extract(agent: AgentKind, raw: &Value) -> Extracted {
    let Some(object) = raw.as_object() else {
        return Extracted::default();
    };
    let extracted = match agent {
        AgentKind::AntigravityCli => object
            .get("toolCall")
            .and_then(Value::as_object)
            .and_then(|call| Some((call.get("name")?.as_str()?, call.get("args")))),
        AgentKind::Grok => object
            .get("tool_name")
            .or_else(|| object.get("toolName"))
            .and_then(Value::as_str)
            .map(|name| {
                (
                    name,
                    object.get("tool_input").or_else(|| object.get("toolInput")),
                )
            }),
        AgentKind::Zero => object
            .get("toolName")
            .and_then(Value::as_str)
            .map(|name| (name, object.get("input"))),
        AgentKind::ClaudeCode
        | AgentKind::CommandCode
        | AgentKind::Codex
        | AgentKind::Cursor
        | AgentKind::GeminiCli
        | AgentKind::Devin
        | AgentKind::Hermes
        | AgentKind::KiroCli
        | AgentKind::Pool
        // ZCode mirrors Claude Code's snake_case `tool_name`/`tool_input`
        // aliases on every tool event (live-captured, #512).
        | AgentKind::Zcode => object
            .get("tool_name")
            .and_then(Value::as_str)
            .map(|name| (name, object.get("tool_input"))),
        AgentKind::OpenCode | AgentKind::Omp | AgentKind::Pi | AgentKind::OpenClaw => object
            .get("tool")
            .and_then(Value::as_str)
            .map(|name| (name, object.get("args"))),
        _ => None,
    };
    let Some((name, args)) = extracted else {
        return Extracted::default();
    };
    let family = family(name);
    let paths = (family == ToolFamily::File)
        .then(|| args.and_then(|value| extract_paths(name, value)))
        .flatten();
    let state = if family == ToolFamily::File && paths.is_none() {
        ExtractionState::MissingOrMalformed
    } else {
        ExtractionState::Extracted
    };
    let call_id = [
        "tool_use_id",
        "toolUseId",
        "tool_call_id",
        "toolCallId",
        "call_id",
        "callId",
        "callID",
    ]
    .iter()
    .find_map(|key| {
        object
            .get(*key)
            .and_then(Value::as_str)
            .filter(|id| valid_call_id(id))
            .map(str::to_owned)
    });
    Extracted {
        family,
        paths,
        call_id,
        state,
    }
}

fn family(name: &str) -> ToolFamily {
    match name.to_ascii_lowercase().as_str() {
        "read"
        | "write"
        | "edit"
        | "apply_patch"
        | "notebookedit"
        | "notebook_edit"
        | "create_file"
        | "delete_file"
        | "remove"
        | "rename_file"
        | "move_file"
        | "multi_edit"
        | "multiedit"
        | "replace"
        | "replace_all"
        | "view_file"
        | "replace_file_content"
        | "multi_replace_file_content"
        | "write_to_file"
        | "fs_read"
        | "fs_write" => ToolFamily::File,
        "read_file" | "write_file" | "edit_file" | "patch" => ToolFamily::File,
        "search" | "grep" | "glob" | "find" | "list" | "ls" | "list_files" | "read_dir"
        | "list_dir" | "grep_search" | "search_files" => ToolFamily::SearchList,
        "bash" | "shell" | "shell_command" | "execute" | "run_command" | "web_search"
        | "terminal" | "execute_bash" | "execute_cmd" => ToolFamily::NonFile,
        _ => ToolFamily::Unknown,
    }
}
fn canonical(family: ToolFamily) -> CanonicalTool {
    match family {
        ToolFamily::File => CanonicalTool::File,
        ToolFamily::SearchList => CanonicalTool::SearchList,
        ToolFamily::NonFile => CanonicalTool::NonFile,
        ToolFamily::Unknown => CanonicalTool::Unknown,
    }
}
pub(crate) fn valid_call_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_CALL_ID_CHARS
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
}

fn extract_paths(name: &str, args: &Value) -> Option<Vec<String>> {
    let object = args.as_object()?;
    let mut paths = direct_paths(object).unwrap_or_default();
    if matches!(
        name.to_ascii_lowercase().as_str(),
        "multi_edit" | "multiedit" | "replace_all"
    ) {
        let entries = object
            .get("edits")
            .or_else(|| object.get("replacements"))?
            .as_array()?;
        if entries.is_empty() {
            return None;
        }
        if entries.len() > MAX_CAPTURE_CANDIDATES {
            return None;
        }
        for entry in entries {
            let paths_in_entry = direct_paths(entry.as_object()?)?;
            if paths.len().checked_add(paths_in_entry.len())? > MAX_CAPTURE_CANDIDATES {
                return None;
            }
            paths.extend(paths_in_entry);
        }
    }
    if matches!(name.to_ascii_lowercase().as_str(), "read" | "fs_read")
        && let Some(operations) = object.get("operations")
    {
        let entries = operations.as_array()?;
        if entries.is_empty() || entries.len() > MAX_CAPTURE_CANDIDATES {
            return None;
        }
        for entry in entries {
            let paths_in_entry = direct_paths(entry.as_object()?)?;
            if paths.len().checked_add(paths_in_entry.len())? > MAX_CAPTURE_CANDIDATES {
                return None;
            }
            paths.extend(paths_in_entry);
        }
    }
    (!paths.is_empty()
        && paths.iter().all(|path| {
            !path.trim().is_empty() && path.chars().count() <= MAX_CANDIDATE_PATH_CHARS
        }))
    .then_some(paths)
}
fn direct_paths(object: &Map<String, Value>) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    for key in [
        "file_path",
        "filePath",
        "path",
        "absolute_path",
        "AbsolutePath",
        "notebook_path",
        "TargetFile",
    ] {
        if let Some(value) = object.get(key) {
            if paths.len() == MAX_CAPTURE_CANDIDATES {
                return None;
            }
            paths.push(value.as_str()?.to_owned());
        }
    }
    if let Some(values) = object.get("paths") {
        if values.as_array()?.len() > MAX_CAPTURE_CANDIDATES
            || paths.len().checked_add(values.as_array()?.len())? > MAX_CAPTURE_CANDIDATES
        {
            return None;
        }
        for value in values.as_array()? {
            paths.push(value.as_str()?.to_owned());
        }
    }
    (!paths.is_empty()).then_some(paths)
}

#[derive(Clone)]
struct Normalized {
    path: String,
    flavor: Flavor,
}
fn normalize_candidate(candidate: &str, cwd: &str) -> Option<Normalized> {
    if candidate.trim().is_empty()
        || candidate.chars().count() > MAX_CANDIDATE_PATH_CHARS
        || is_drive_relative(candidate)
    {
        return None;
    }
    let cwd = normalize_root(cwd).ok()?;
    let raw = if is_absolute(candidate) {
        candidate.to_owned()
    } else {
        join(&cwd, candidate)
    };
    let flavor = flavor_of(&raw);
    Some(Normalized {
        path: normalize_segments(&raw)?,
        flavor,
    })
}
fn compile(
    config: &CaptureConfig,
    marker_dir: &str,
    home_dir: Option<&str>,
) -> Result<Vec<CompiledPattern>, ()> {
    if config.ignore_paths.len() > MAX_IGNORE_PATTERNS {
        return Err(());
    }
    let marker = normalize_root(marker_dir).map_err(|_| ())?;
    let needs_home = config
        .ignore_paths
        .iter()
        .any(|pattern| pattern.starts_with("~/"));
    let home = if needs_home {
        Some(
            home_dir
                .and_then(|dir| normalize_root(dir).ok())
                .ok_or(())?,
        )
    } else {
        None
    };
    config
        .ignore_paths
        .iter()
        .map(|source| {
            validate_glob(source)?;
            let expanded = if let Some(rest) = source.strip_prefix("~/") {
                join(home.as_deref().ok_or(())?, rest)
            } else if is_absolute(source) {
                source.clone()
            } else {
                join(&marker, source)
            };
            let flavor = flavor_of(&expanded);
            let path = normalize_segments(&expanded).ok_or(())?;
            Ok(CompiledPattern {
                directory_base: path.strip_suffix("/**").map(|base| {
                    if base.is_empty() {
                        "/".into()
                    } else {
                        base.into()
                    }
                }),
                path,
                flavor,
            })
        })
        .collect()
}
fn validate_glob(pattern: &str) -> Result<(), ()> {
    if pattern.trim().is_empty()
        || pattern.chars().count() > MAX_IGNORE_PATTERN_CHARS
        || pattern.contains(['!', '{', '}', '[', ']', '(', ')', '|', '^', '$', '%'])
        || pattern.contains("${")
        || pattern.contains("***")
        || pattern
            .replace('\\', "/")
            .split('/')
            .any(|segment| segment == "..")
        || (pattern.starts_with('~') && !pattern.starts_with("~/"))
        || is_drive_relative(pattern)
    {
        return Err(());
    }
    Ok(())
}
fn join(base: &str, child: &str) -> String {
    format!("{}/{}", base.trim_end_matches(['/', '\\']), child)
}
fn flavor_of(path: &str) -> Flavor {
    if path.starts_with("\\\\") || path.starts_with("//") || valid_drive_prefix(path) {
        Flavor::Windows
    } else {
        Flavor::Posix
    }
}
fn valid_drive_prefix(path: &str) -> bool {
    path.len() >= 2 && path.as_bytes()[0].is_ascii_alphabetic() && path.as_bytes()[1] == b':'
}
fn is_absolute(path: &str) -> bool {
    path.starts_with('/')
        || path.starts_with("\\\\")
        || (valid_drive_prefix(path)
            && path.len() >= 3
            && matches!(path.as_bytes()[2], b'/' | b'\\'))
}
fn is_drive_relative(path: &str) -> bool {
    path.len() >= 2
        && path.as_bytes()[1] == b':'
        && (!path.as_bytes()[0].is_ascii_alphabetic() || !is_absolute(path))
}
fn normalize_root(path: &str) -> Result<String, ()> {
    (is_absolute(path) && !is_drive_relative(path))
        .then(|| normalize_segments(path))
        .flatten()
        .ok_or(())
}
fn normalize_segments(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    let (root, tail): (String, Vec<&str>) = if let Some(rest) = path.strip_prefix("//") {
        let mut parts = rest.split('/').filter(|p| !p.is_empty());
        let server = parts.next()?;
        let share = parts.next()?;
        (format!("//{server}/{share}"), parts.collect())
    } else if valid_drive_prefix(&path) && path.as_bytes().get(2) == Some(&b'/') {
        (
            format!("{}:/", path[..1].to_ascii_uppercase()),
            path[3..].split('/').collect(),
        )
    } else {
        let rest = path.strip_prefix('/')?;
        ("/".into(), rest.split('/').collect())
    };
    let mut parts = Vec::new();
    for part in tail {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            parts.pop();
        } else {
            parts.push(part);
        }
    }
    let mut output = root;
    if !parts.is_empty() {
        if !output.ends_with('/') {
            output.push('/');
        }
        output.push_str(&parts.join("/"));
    }
    Some(output)
}
fn glob_match(
    pattern: &str,
    candidate: &str,
    directory_base: Option<&str>,
    insensitive: bool,
) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let candidate: Vec<char> = candidate.chars().collect();
    if directory_base
        .is_some_and(|base| equal_chars(&base.chars().collect::<Vec<_>>(), &candidate, insensitive))
    {
        return true;
    }
    let mut previous = vec![false; pattern.len() + 1];
    previous[0] = true;
    for index in 1..=pattern.len() {
        previous[index] = match pattern[index - 1] {
            '*' if pattern.get(index) == Some(&'*') => false,
            '*' => previous[index - 1],
            _ => false,
        };
    }
    for character in candidate {
        let mut current = vec![false; pattern.len() + 1];
        for index in 1..=pattern.len() {
            current[index] = match pattern[index - 1] {
                '*' if pattern.get(index) == Some(&'*') => false,
                '*' if index >= 2 && pattern[index - 2] == '*' => {
                    current[index - 2] || previous[index]
                }
                '*' => current[index - 1] || (character != '/' && previous[index]),
                '?' => character != '/' && previous[index - 1],
                expected => char_equal(expected, character, insensitive) && previous[index - 1],
            };
        }
        previous = current;
    }
    previous[pattern.len()]
}
fn equal_chars(left: &[char], right: &[char], insensitive: bool) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(&a, &b)| char_equal(a, b, insensitive))
}
fn char_equal(left: char, right: char, insensitive: bool) -> bool {
    if insensitive && left.is_ascii() && right.is_ascii() {
        left.eq_ignore_ascii_case(&right)
    } else {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denylist_admits_every_repository() {
        assert!(repository_admits_capture(CaptureMode::Denylist, false));
        assert!(repository_admits_capture(CaptureMode::Denylist, true));
    }

    #[test]
    fn allowlist_admits_only_a_marked_repository() {
        assert!(!repository_admits_capture(CaptureMode::Allowlist, false));
        assert!(repository_admits_capture(CaptureMode::Allowlist, true));
    }

    #[test]
    fn default_mode_is_the_historical_one() {
        // A new field must not silently tighten capture for existing installs.
        assert_eq!(CaptureMode::default(), CaptureMode::Denylist);
    }
    #[test]
    fn fixture_vectors() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/capture-policy.json")).unwrap();
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secret/**".into()],
            }),
            "/repo",
            None,
        );
        for vector in fixture["decisions"].as_array().unwrap() {
            let agent = AgentKind::from_wire(vector["agent"].as_str().unwrap());
            let decision = policy.inspect(agent, &vector["payload"], "/repo");
            let protocol = decision.protocol();
            assert_eq!(
                serde_json::to_value(protocol.disposition()).unwrap(),
                vector["disposition"]
            );
            assert_eq!(
                serde_json::to_value(protocol.tool_family()).unwrap(),
                vector["tool_family"]
            );
            assert_eq!(
                serde_json::to_value(protocol.extraction_state()).unwrap(),
                vector["extraction_state"]
            );
            assert_eq!(
                protocol.path_count(),
                vector["path_count"].as_u64().unwrap() as u16
            );
        }
        for vector in fixture["normalization"].as_array().unwrap() {
            let policy = CapturePolicy::resolve(
                CaptureSource::Parsed(&CaptureConfig {
                    ignore_paths: vec![vector["pattern"].as_str().unwrap().into()],
                }),
                "/repo",
                None,
            );
            assert_eq!(
                policy
                    .match_paths(
                        &[vector["candidate"].as_str().unwrap().into()],
                        vector["cwd"].as_str().unwrap()
                    )
                    .unwrap(),
                vector["match"].as_bool().unwrap()
            );
        }
        assert!(CaptureProtocol::parse(&fixture["protocol"]["accept"]).is_some());
        assert!(CaptureProtocol::parse(&fixture["protocol"]["reject"]).is_none());
    }
    #[test]
    fn all_states_and_strict_protocol_are_reachable() {
        let file = json!({"tool_name":"Edit","tool_input":{}});
        assert_eq!(
            CapturePolicy::resolve(CaptureSource::Absent, "/repo", None)
                .inspect(AgentKind::Codex, &file, "/repo")
                .protocol()
                .policy_state(),
            PolicyState::Inactive
        );
        let invalid = CapturePolicy::resolve(CaptureSource::Invalid, "/repo", None).inspect(
            AgentKind::Codex,
            &file,
            "/repo",
        );
        assert_eq!(invalid.protocol().policy_state(), PolicyState::Invalid);
        assert_eq!(
            invalid.protocol().disposition(),
            CaptureDisposition::MetadataOnly
        );
        assert_eq!(
            CapturePolicy::resolve(
                CaptureSource::Parsed(&CaptureConfig {
                    ignore_paths: vec!["a/../b".into()]
                }),
                "/repo",
                None
            )
            .inspect(AgentKind::Codex, &file, "/repo")
            .protocol()
            .policy_state(),
            PolicyState::Invalid
        );
        assert_eq!(
            CapturePolicy::resolve(
                CaptureSource::Parsed(&CaptureConfig {
                    ignore_paths: vec!["x".into()]
                }),
                "/repo",
                None
            )
            .inspect(AgentKind::Other, &json!({}), "/repo")
            .protocol()
            .extraction_state(),
            ExtractionState::UnsupportedSchema
        );
        assert!(
            serde_json::from_value::<CaptureConfig>(json!({"ignore_paths": [], "mode": "keep"}))
                .is_err()
        );
        let value = serde_json::to_value(invalid.protocol()).unwrap();
        assert!(CaptureProtocol::parse(&value).is_some());
        let mut bad = value;
        bad.as_object_mut()
            .unwrap()
            .insert("paths".into(), json!(["x"]));
        assert!(CaptureProtocol::parse(&bad).is_none());
    }

    #[test]
    fn hermes_official_tool_shape_is_closed_and_honors_exclusions() {
        let raw = json!({
            "hook_event_name": "post_tool_call",
            "tool_name": "write_file",
            "tool_input": {"path": "secret/token.txt", "content": "do not retain"},
            "session_id": "hermes-session",
            "cwd": "/repo",
            "extra": {"tool_call_id": "call-42", "status": "ok"}
        });
        let metadata = tool_observation_metadata(AgentKind::Hermes, &raw, false).unwrap();
        assert_eq!(metadata.tool_family, ToolFamily::File);
        assert_eq!(metadata.tool_call_id.as_deref(), Some("call-42"));

        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secret/**".into()],
            }),
            "/repo",
            None,
        );
        let decision = policy.inspect(AgentKind::Hermes, &raw, "/repo");
        assert_eq!(decision.protocol().tool_family(), ToolFamily::File);
        assert_eq!(decision.protocol().disposition(), CaptureDisposition::Drop);

        let unknown = policy.inspect(AgentKind::Other, &raw, "/repo");
        assert_eq!(
            unknown.protocol().extraction_state(),
            ExtractionState::UnsupportedSchema
        );
        assert_eq!(unknown.protocol().tool_family(), ToolFamily::Unknown);
    }

    #[test]
    fn hermes_documented_tool_names_map_to_canonical_families() {
        for (tool, expected) in [
            ("read_file", ToolFamily::File),
            ("write_file", ToolFamily::File),
            ("patch", ToolFamily::File),
            ("search_files", ToolFamily::SearchList),
            ("terminal", ToolFamily::NonFile),
        ] {
            assert_eq!(family(tool), expected, "tool: {tool}");
        }
    }

    #[test]
    fn pool_documented_tool_shape_is_closed_and_honors_exclusions() {
        let raw = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "write",
            "tool_input": {"path": "secret/token.txt", "content": "do not retain"},
            "session_id": "pool-session",
            "cwd": "/repo"
        });
        let metadata = tool_observation_metadata(AgentKind::Pool, &raw, true).unwrap();
        assert_eq!(metadata.tool_family, ToolFamily::File);
        assert_eq!(metadata.tool_call_id, None);

        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secret/**".into()],
            }),
            "/repo",
            None,
        );
        let decision = policy.inspect(AgentKind::Pool, &raw, "/repo");
        assert_eq!(decision.protocol().tool_family(), ToolFamily::File);
        assert_eq!(decision.protocol().disposition(), CaptureDisposition::Drop);

        let unknown = policy.inspect(AgentKind::Other, &raw, "/repo");
        assert_eq!(
            unknown.protocol().extraction_state(),
            ExtractionState::UnsupportedSchema
        );
        assert_eq!(unknown.protocol().tool_family(), ToolFamily::Unknown);
    }

    #[test]
    fn pool_documented_tool_names_map_to_canonical_families() {
        for (tool, expected) in [
            ("read", ToolFamily::File),
            ("edit", ToolFamily::File),
            ("write", ToolFamily::File),
            ("remove", ToolFamily::File),
            ("shell", ToolFamily::NonFile),
        ] {
            assert_eq!(family(tool), expected, "tool: {tool}");
        }
    }

    #[test]
    fn zcode_documented_tool_shape_is_closed_and_honors_exclusions() {
        // Live-captured ZCode payload (engine v0.16.5, #512): the snake_case
        // aliases carry exactly the fields Claude Code sends, including a
        // real provider-format tool call id (`call_…`).
        let raw = json!({
            "hookEventName": "PostToolUse",
            "toolName": "Write",
            "tool_name": "Write",
            "tool_use_id": "call_6d8f8fd5d9eb4888b0f9d5c6",
            "toolInput": {"file_path": "/repo/secret/token.txt", "content": "do not retain"},
            "tool_input": {"file_path": "/repo/secret/token.txt", "content": "do not retain"},
            "session_id": "sess_0a5ba797",
            "cwd": "/repo"
        });
        let metadata = tool_observation_metadata(AgentKind::Zcode, &raw, true).unwrap();
        assert_eq!(metadata.tool_family, ToolFamily::File);
        assert_eq!(
            metadata.tool_call_id.as_deref(),
            Some("call_6d8f8fd5d9eb4888b0f9d5c6")
        );

        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secret/**".into()],
            }),
            "/repo",
            None,
        );
        let decision = policy.inspect(AgentKind::Zcode, &raw, "/repo");
        assert_eq!(decision.protocol().tool_family(), ToolFamily::File);
        assert_eq!(decision.protocol().disposition(), CaptureDisposition::Drop);

        let unknown = policy.inspect(AgentKind::Other, &raw, "/repo");
        assert_eq!(
            unknown.protocol().extraction_state(),
            ExtractionState::UnsupportedSchema
        );
        assert_eq!(unknown.protocol().tool_family(), ToolFamily::Unknown);
    }

    #[test]
    fn zcode_post_tool_use_failure_error_maps_to_error_outcome() {
        // Live-captured PostToolUseFailure payload (run 10, real model, #512):
        // `error` is a plain string, `error_details` carries the internal
        // error class.
        let raw = json!({
            "hook_event_name": "PostToolUseFailure",
            "tool_name": "Write",
            "error": "File not found: /proc/capture-test.txt",
            "error_details": {
                "message": "File not found: /proc/capture-test.txt",
                "type": "FileSystemPortError"
            }
        });
        assert_eq!(
            tool_observation_outcome(AgentKind::Zcode, &raw),
            ToolOutcome::Error
        );
        // exitCode is deliberately not trusted for Success: the same
        // tool_response object carries timedOut / interrupted flags.
        let ok = json!({"tool_response": {"exitCode": 0}});
        assert_eq!(
            tool_observation_outcome(AgentKind::Zcode, &ok),
            ToolOutcome::Unknown
        );
    }

    #[test]
    fn command_code_official_tool_shape_is_closed_and_honors_exclusions() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secret/**".into()],
            }),
            "/repo",
            None,
        );
        let ignored = json!({
            "session_id": "command-session",
            "cwd": "/repo",
            "tool_use_id": "tool-42",
            "tool_name": "edit_file",
            "tool_input": {
                "file_path": "secret/token.txt",
                "old_value": "old",
                "new_value": "new"
            }
        });

        let metadata = tool_observation_metadata(AgentKind::CommandCode, &ignored, true).unwrap();
        assert_eq!(metadata.tool_family, ToolFamily::File);
        assert_eq!(metadata.tool_call_id.as_deref(), Some("tool-42"));
        let decision = policy.inspect(AgentKind::CommandCode, &ignored, "/repo");
        assert_eq!(decision.protocol().disposition(), CaptureDisposition::Drop);

        for (tool, expected) in [
            ("read_file", ToolFamily::File),
            ("write_file", ToolFamily::File),
            ("edit_file", ToolFamily::File),
            ("shell_command", ToolFamily::NonFile),
        ] {
            assert_eq!(family(tool), expected, "tool: {tool}");
        }
    }

    #[test]
    fn kiro_v3_documented_and_live_tool_shapes_honor_exclusions_and_fail_closed() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secret/**".into()],
            }),
            "/repo",
            None,
        );
        let official_read = json!({
            "session_id": "kiro-session",
            "cwd": "/repo",
            "tool_name": "read",
            "tool_input": {
                "operations": [
                    {"mode": "Line", "path": "src/lib.rs"},
                    {"mode": "Line", "path": "secret/token.txt"}
                ]
            }
        });
        let decision = policy.inspect(AgentKind::KiroCli, &official_read, "/repo");
        assert_eq!(decision.protocol().tool_family(), ToolFamily::File);
        assert_eq!(decision.protocol().path_count(), 2);
        assert_eq!(decision.protocol().disposition(), CaptureDisposition::Drop);

        let live_read = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "kiro-session",
            "cwd": "/repo",
            "tool_name": "read_file",
            "tool_input": {
                "path": "secret/token.txt",
                "offset": null,
                "limit": null
            }
        });
        let decision = policy.inspect(AgentKind::KiroCli, &live_read, "/repo");
        assert_eq!(decision.protocol().tool_family(), ToolFamily::File);
        assert_eq!(decision.protocol().path_count(), 1);
        assert_eq!(decision.protocol().disposition(), CaptureDisposition::Drop);

        let mut official_post = official_read.clone();
        official_post.as_object_mut().unwrap().insert(
            "tool_response".to_string(),
            json!({"success": true, "result": ["sanitized"]}),
        );
        assert_eq!(
            tool_observation_outcome(AgentKind::KiroCli, &official_post),
            ToolOutcome::Success
        );

        let unknown_shape = json!({
            "session_id": "kiro-session",
            "cwd": "/repo",
            "tool_name": "read",
            "tool_input": {"files": ["secret/token.txt"]}
        });
        let decision = policy.inspect(AgentKind::KiroCli, &unknown_shape, "/repo");
        assert_eq!(
            decision.protocol().extraction_state(),
            ExtractionState::MissingOrMalformed
        );
        assert_eq!(
            decision.protocol().disposition(),
            CaptureDisposition::MetadataOnly
        );
    }

    #[test]
    fn antigravity_native_tools_are_no_longer_unknown() {
        for (tool, expected) in [
            ("view_file", ToolFamily::File),
            ("replace_file_content", ToolFamily::File),
            ("multi_replace_file_content", ToolFamily::File),
            ("write_to_file", ToolFamily::File),
            ("list_dir", ToolFamily::SearchList),
            ("grep_search", ToolFamily::SearchList),
        ] {
            assert_eq!(family(tool), expected, "tool: {tool}");
        }
    }
    #[test]
    fn antigravity_target_file_is_a_proven_path() {
        let target = json!({"TargetFile": "/repo/src/main.rs"});
        assert_eq!(
            direct_paths(target.as_object().unwrap()).unwrap(),
            vec!["/repo/src/main.rs".to_string()]
        );
    }
    #[test]
    fn antigravity_file_tools_honor_capture_exclusions() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secret/**".into()],
            }),
            "/repo",
            None,
        );
        for tool in [
            "view_file",
            "write_to_file",
            "replace_file_content",
            "multi_replace_file_content",
        ] {
            let ignored = json!({
                "toolCall": {
                    "name": tool,
                    "args": {"TargetFile": "secret/keys.txt"}
                }
            });
            let decision = policy.inspect(AgentKind::AntigravityCli, &ignored, "/repo");
            assert_eq!(decision.protocol().tool_family(), ToolFamily::File);
            assert_eq!(
                decision.protocol().disposition(),
                CaptureDisposition::Drop,
                "tool: {tool}"
            );
        }

        let kept =
            json!({"toolCall": {"name": "view_file", "args": {"TargetFile": "src/main.rs"}}});
        let decision = policy.inspect(AgentKind::AntigravityCli, &kept, "/repo");
        assert_eq!(decision.protocol().tool_family(), ToolFamily::File);
        assert_eq!(decision.protocol().disposition(), CaptureDisposition::Keep);
    }
    #[test]
    fn normalization_and_matcher_bounds() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec![
                    r"C:\\Secret\\**".into(),
                    r"\\server\\share\\x".into(),
                    "unicode/?.txt".into(),
                ],
            }),
            "/repo",
            None,
        );
        assert!(policy.match_paths(&[r"c:/SECRET".into()], "C:/").unwrap());
        assert!(
            policy
                .match_paths(&[r"\\server\\share\\x".into()], "C:/")
                .unwrap()
        );
        assert!(
            policy
                .match_paths(&["unicode/é.txt".into()], "/repo")
                .unwrap()
        );
        assert!(policy.match_paths(&["C:bad".into()], "C:/").is_err());
        let raw = json!({"tool_name":"Edit","tool_input":{"path":"x".repeat(MAX_CANDIDATE_PATH_CHARS + 1)}});
        assert_eq!(
            policy
                .inspect(AgentKind::Codex, &raw, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::MetadataOnly
        );
        let overflow = json!({"tool_name":"Edit","tool_input":{"paths":vec!["public"; MAX_CAPTURE_CANDIDATES + 1]}});
        assert_eq!(
            policy
                .inspect(AgentKind::Codex, &overflow, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::MetadataOnly
        );
        let costly = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["?".repeat(MAX_IGNORE_PATTERN_CHARS)],
            }),
            "/repo",
            None,
        );
        let costly_raw =
            json!({"tool_name":"Edit","tool_input":{"path":"x".repeat(MAX_CANDIDATE_PATH_CHARS)}});
        assert_eq!(
            costly
                .inspect(AgentKind::Codex, &costly_raw, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::MetadataOnly
        );
        let chars = "?".repeat(MAX_IGNORE_PATTERN_CHARS);
        assert_eq!(CapturePolicy::resolve(CaptureSource::Parsed(&CaptureConfig { ignore_paths: vec![chars] }), "/repo", None).inspect(AgentKind::Codex, &json!({"tool_name":"Edit","tool_input":{"path":"x".repeat(MAX_IGNORE_PATTERN_CHARS)}}), "/repo").protocol().policy_state(), PolicyState::Active);
        assert_eq!(
            CapturePolicy::resolve(
                CaptureSource::Parsed(&CaptureConfig {
                    ignore_paths: vec!["?".repeat(MAX_IGNORE_PATTERN_CHARS + 1)]
                }),
                "/repo",
                None
            )
            .inspect(
                AgentKind::Codex,
                &json!({"tool_name":"Edit","tool_input":{"path":"x"}}),
                "/repo"
            )
            .protocol()
            .policy_state(),
            PolicyState::Invalid
        );
    }
    #[test]
    fn metadata_rewrite_strips_all_sentinels_and_has_exact_protocol_keys() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secret/**".into()],
            }),
            "/repo",
            None,
        );
        let raw = json!({"tool_name":"Edit","tool_call_id":"safe-ID.1","tool_input":{"path":"secret/SENTINEL_PATH","args":"SENTINEL_ARGS"},"output":"SENTINEL_OUTPUT","error":"SENTINEL_ERROR","title":"SENTINEL_TITLE","result":"SENTINEL_RESULT","nested":{"raw":"SENTINEL_NESTED"}});
        let body = metadata_only_body(
            Some("s"),
            Some("/repo"),
            &policy.inspect(AgentKind::Codex, &raw, "/repo"),
        );
        let text = body.to_string();
        for sentinel in [
            "SENTINEL_PATH",
            "SENTINEL_ARGS",
            "SENTINEL_OUTPUT",
            "SENTINEL_ERROR",
            "SENTINEL_TITLE",
            "SENTINEL_RESULT",
            "SENTINEL_NESTED",
        ] {
            assert!(!text.contains(sentinel));
        }
        let keys: Vec<_> = body["_ai_memory_capture"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            keys,
            [
                "version",
                "disposition",
                "policy_state",
                "tool_family",
                "path_count",
                "extraction_state"
            ]
        );
    }
}
