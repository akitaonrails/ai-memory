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
/// Longest argv element kept whole as a path; longer ones are scripts.
const MAX_ARGV_PATH_CHARS: usize = 256;
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
    /// No path extraction applied to this event. Two cases produce it:
    ///
    /// - An active policy met a search/list tool ([`ToolFamily::SearchList`]).
    ///   The policy drops it whatever its arguments say, so no path is
    ///   extracted or counted. Under an inactive or invalid policy the same
    ///   tool reports [`Self::Extracted`]: the state depends on the policy,
    ///   not only on the tool.
    /// - `ai-memory hook --check-capture` evaluated no policy at all, because
    ///   the event is not a tool event or no working directory was found. The
    ///   report then pairs it with `policy_state: inactive` and
    ///   `tool_family: unknown`.
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
    /// Set only for an unknown-family call whose name is a strictly
    /// validated MCP tool name; see [`mcp_tool_name`].
    pub(crate) mcp_tool: Option<McpToolName>,
}

/// The server and tool an MCP tool name identifies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct McpToolName {
    pub(crate) server: String,
    pub(crate) tool: String,
}

/// Longest MCP tool name kept. Hermes clamps its generated names to the 64
/// characters OpenAI-compatible providers accept (`tools/mcp_tool_schema.py`,
/// v0.22.2); a longer name is not a shape any documented harness sends.
const MAX_MCP_TOOL_NAME_CHARS: usize = 64;

/// The server and tool of an `mcp__<server>__<tool>` tool name, the shape
/// Claude Code (<https://code.claude.com/docs/en/hooks>, "Match MCP tools",
/// plugin servers as `mcp__plugin_<plugin>_<server>__<tool>`), Codex
/// (<https://learn.chatgpt.com/docs/hooks>, `mcp__fs__read`) and Hermes Agent
/// (`mcp_prefixed_tool_name`, v0.22.2; server and tool reduced to
/// `[A-Za-z0-9_]`) put in a tool hook's `tool_name`.
///
/// An MCP tool's arguments have no known path shape, so it stays an unknown
/// family and keeps no input or output. Only the two names are kept, and only
/// when the whole name is at most 64 ASCII letters, digits, `_` or `-`: an
/// identifier, never free text. The server ends at the first `__`.
pub(crate) fn mcp_tool_name(name: &str) -> Option<McpToolName> {
    if name.len() > MAX_MCP_TOOL_NAME_CHARS
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    let (server, tool) = name.strip_prefix("mcp__")?.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then(|| McpToolName {
        server: server.to_owned(),
        tool: tool.to_owned(),
    })
}

/// Longest harness tool name kept by `[ingest] record_tool_names`, the same
/// bound as an MCP tool name: every documented harness tool name fits.
const MAX_TOOL_NAME_CHARS: usize = 64;

/// Longest skill or subagent identifier kept by `[ingest] record_tool_names`.
/// Plugin skills carry a `plugin:skill` prefix, so this is wider than a name.
const MAX_TOOL_DESCRIPTOR_CHARS: usize = 128;

/// A closed-tool call's own name, plus the one identifier that says which
/// skill or subagent it ran. Kept only when the operator opts in with
/// `[ingest] record_tool_names`; [`crate::HookEnvelope::record_tool_name`]
/// writes it into the observation.
///
/// Never an argument. A shell command, a prompt, a path or a URL stays out:
/// the name and the two descriptors are kept only when they are short
/// identifiers, so free text a harness put in those fields is refused whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecordedToolName {
    pub(crate) name: String,
    /// `tool_input.skill` of Claude Code's `Skill` tool.
    pub(crate) skill: Option<String>,
    /// `tool_input.subagent_type` of Claude Code's `Agent` (formerly `Task`)
    /// tool.
    pub(crate) subagent_type: Option<String>,
}

/// The tool name a closed-tool event carries, read from the same adapter
/// fields [`tool_observation_metadata`] uses, and refused unless it is at most
/// 64 ASCII letters, digits, `_` or `-`.
///
/// The skill and subagent identifiers are Claude Code's own `tool_input`
/// fields (<https://code.claude.com/docs/en/hooks>, "PreToolUse input": the
/// `Agent` tool's `subagent_type`; the `Skill` tool's `skill`), so they are
/// read for Claude Code only. Each is kept when it is at most 128 ASCII
/// letters, digits, `_`, `-`, `.` or `:`.
pub(crate) fn recorded_tool_name(agent: AgentKind, raw: &Value) -> Option<RecordedToolName> {
    let (name, _) = tool_payload_fields(agent, raw)?;
    if !valid_tool_name(name) {
        return None;
    }
    let input = if agent == AgentKind::ClaudeCode {
        raw.get("tool_input")
    } else {
        None
    };
    let descriptor = |key: &str| {
        input
            .and_then(|input| input.get(key))
            .and_then(Value::as_str)
            .filter(|value| valid_tool_descriptor(value))
            .map(str::to_owned)
    };
    Some(RecordedToolName {
        name: name.to_owned(),
        skill: (name == "Skill").then(|| descriptor("skill")).flatten(),
        subagent_type: matches!(name, "Agent" | "Task")
            .then(|| descriptor("subagent_type"))
            .flatten(),
    })
}

/// A harness tool name as `[ingest] record_tool_names` keeps it: an
/// identifier, never free text.
pub(crate) fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_TOOL_NAME_CHARS
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn valid_tool_descriptor(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOOL_DESCRIPTOR_CHARS
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
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

/// The raw tool name and tool-call id of a tool payload, per agent.
///
/// Both the tool observation metadata and the server-side drop list (#1241)
/// read these fields, so they share one mapping: an agent added here is
/// filterable by the drop list without a second field table to keep in step.
/// `None` when the agent has no closed tool-payload shape or the payload lacks
/// the name; the id is independently optional.
pub(crate) fn tool_payload_fields(agent: AgentKind, raw: &Value) -> Option<(&str, Option<&str>)> {
    let object = raw.as_object()?;
    let (name, id) = match agent {
        // ZCode tool payloads carry Claude Code's snake_case aliases
        // (`tool_name`, `tool_use_id`, `tool_input`) alongside the native
        // camelCase — all captured live (engine v0.16.5, #512).
        // Native Codex 0.154 uses these same top-level fields, including
        // `tool_use_id` on both sides of a tool call.
        // Grok Build CLI posts Claude Code's snake_case aliases
        // (`tool_name` / `tool_input` / `tool_use_id`) on its tool hooks
        // alongside camelCase, so it shares this mapping (#931).
        // GitHub Copilot CLI, configured with PascalCase hook event names per
        // its own docs, emits the same Claude-compatible `tool_name`/
        // `tool_input` shape (#1040) — unverified against a real tool-call id
        // field, but `tool_use_id` extraction degrades to `None` gracefully
        // if absent.
        // Cursor's own hooks and its copies of the Claude Code hooks carry
        // `tool_name`/`tool_input`/`tool_use_id` (captured live, Cursor 3.24.9).
        // Meta Muse Code sends the same three fields, `tool_use_id` being the
        // provider's call id (`call_…`, captured offline on Muse Code 1.4.7).
        // CodeBuddy Code sends the same Claude Code fields (#1299).
        // DSH's Claude Code hooks bridge forwards the same Claude Code
        // snake_case fields for PreToolUse / PostToolUse.
        AgentKind::ClaudeCode
        | AgentKind::CommandCode
        | AgentKind::Codex
        | AgentKind::Cursor
        | AgentKind::Grok
        | AgentKind::Grizzybot
        | AgentKind::Zcode
        | AgentKind::CopilotCli
        | AgentKind::Muse
        | AgentKind::CodeBuddy
        | AgentKind::IbmBob
        | AgentKind::Dsh
        | AgentKind::QwenCode
        | AgentKind::Qoder => (
            object.get("tool_name")?.as_str()?,
            object.get("tool_use_id").and_then(Value::as_str),
        ),
        AgentKind::OpenCode => (
            object.get("tool")?.as_str()?,
            object.get("callID").and_then(Value::as_str),
        ),
        // The Pi extension is generated from the OMP one (`build_pi_extension`),
        // so both post the same `tool` / `callID` / `args` payload.
        AgentKind::Pi | AgentKind::Omp => (
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
        // Factory Droid sends Claude Code's `tool_name` + `tool_input` but no
        // tool-call id at all: Droid tracks its call ids internally and none
        // reaches a hook payload (verified against the 0.240.0 tool schemas,
        // #1240).
        AgentKind::Droid => (object.get("tool_name")?.as_str()?, None),
        AgentKind::Cline => {
            let tool = object
                .get("tool_call")
                .or_else(|| object.get("tool_result"))?
                .as_object()?;
            (
                tool.get("name")?.as_str()?,
                tool.get("id").and_then(Value::as_str),
            )
        }
        _ => return None,
    };
    Some((name, id))
}

/// Extracts only the closed, fixture-backed metadata schemas used for tool
/// observations. It intentionally does not reuse broad envelope fallbacks.
pub(crate) fn tool_observation_metadata(
    agent: AgentKind,
    raw: &Value,
    require_input: bool,
) -> Option<ToolObservationMetadata> {
    let (name, id) = tool_payload_fields(agent, raw)?;
    let object = raw.as_object()?;
    // PreToolUse needs a proven input shape. PostToolUse deliberately does
    // not: established adapters commonly omit inputs from their response.
    let has_args = !require_input
        || match agent {
            AgentKind::AntigravityCli => object.get("toolCall")?.get("args").is_some(),
            AgentKind::Cline => object
                .get("tool_call")
                .and_then(Value::as_object)
                .and_then(|tool| tool.get("input"))
                .is_some(),
            _ => object
                .get(
                    if matches!(
                        agent,
                        AgentKind::ClaudeCode
                            | AgentKind::CommandCode
                            | AgentKind::Codex
                            | AgentKind::Cursor
                            | AgentKind::Grok
                            | AgentKind::Hermes
                            | AgentKind::KiroCli
                            | AgentKind::Pool
                            | AgentKind::Zcode
                            | AgentKind::CopilotCli
                            | AgentKind::Grizzybot
                            | AgentKind::Droid
                            | AgentKind::Muse
                            | AgentKind::CodeBuddy
                            | AgentKind::IbmBob
                            | AgentKind::Dsh
                            | AgentKind::QwenCode
                            | AgentKind::Qoder
                    ) {
                        "tool_input"
                    } else {
                        "args"
                    },
                )
                .is_some(),
        };
    has_args.then(|| {
        let tool_family = agent_family(agent, name);
        ToolObservationMetadata {
            tool_family,
            tool_call_id: id.filter(|id| valid_call_id(id)).map(str::to_owned),
            mcp_tool: if tool_family == ToolFamily::Unknown {
                mcp_tool_name(name)
            } else {
                None
            },
        }
    })
}

/// Extracts an outcome only where the adapter protocol proves its meaning.
pub(crate) fn tool_observation_outcome(agent: AgentKind, raw: &Value) -> ToolOutcome {
    match agent {
        AgentKind::Pi | AgentKind::Omp => match raw.get("isError").and_then(Value::as_bool) {
            Some(true) => ToolOutcome::Error,
            Some(false) => ToolOutcome::Success,
            None => ToolOutcome::Unknown,
        },
        AgentKind::Cline => match raw
            .get("tool_result")
            .and_then(Value::as_object)
            .and_then(|tool| tool.get("error"))
        {
            Some(Value::Null) | None => ToolOutcome::Success,
            Some(Value::String(error)) if error.is_empty() => ToolOutcome::Success,
            Some(_) => ToolOutcome::Error,
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
        // Meta Muse Code's `PostToolUseFailure` has the same `error` string
        // (captured offline on Muse Code 1.4.7); its `PostToolUse` proves no
        // outcome.
        // Claude Code's `PostToolUseFailure` carries the same `error` string
        // (<https://code.claude.com/docs/en/hooks>); an aborted call
        // (`is_interrupt`) did not succeed either. Its `PostToolUse` fires
        // only on success and has no `error` key.
        AgentKind::Zcode
        | AgentKind::Muse
        | AgentKind::QwenCode
        | AgentKind::Qoder
        | AgentKind::ClaudeCode
            if raw
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|error| !error.is_empty()) =>
        {
            ToolOutcome::Error
        }
        // GitHub Copilot CLI's VS-Code-compatible `PostToolUse` carries
        // `tool_result.result_type` (#1040). Only `success` and an explicit
        // failure prove an outcome; any other value (a denied or rejected
        // call, a future type) stays unknown. `PostToolUseFailure` is aliased
        // to `PostToolUse`, so an `error` string with no `tool_result` is the
        // failure form, as for ZCode.
        AgentKind::CopilotCli => match raw
            .get("tool_result")
            .and_then(|result| result.get("result_type"))
            .and_then(Value::as_str)
        {
            Some("success") => ToolOutcome::Success,
            Some("failure" | "error") => ToolOutcome::Error,
            Some(_) => ToolOutcome::Unknown,
            None if raw
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|error| !error.is_empty()) =>
            {
                ToolOutcome::Error
            }
            None => ToolOutcome::Unknown,
        },
        // Hermes mirrors the call status in `extra.status` (`ok` | `error`,
        // set by `model_tools.py::_emit_post_tool_call_hook` from the same
        // result the model sees). `ok` proves success and a non-empty
        // `error_message` proves failure; anything else stays unknown.
        AgentKind::Hermes => {
            let status = raw
                .get("extra")
                .and_then(|extra| extra.get("status"))
                .and_then(Value::as_str);
            match status {
                Some("ok") => ToolOutcome::Success,
                Some("error") => ToolOutcome::Error,
                _ => ToolOutcome::Unknown,
            }
        }
        // Codex PostToolUse also fires for failed commands. Its native exec
        // response is output text, with no separate success/exit-code field;
        // neither the event nor arbitrary response JSON proves an outcome.
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
    home: Option<String>,
    /// The marker directory, where git resolves a `REV:path` object name
    /// when the repository root holds the marker.
    root: Option<String>,
    /// POSIX paths compare without regard to case or Unicode composition,
    /// as on macOS, whose default filesystem reads `NOTES/X.md` as
    /// `notes/x.md` and a decomposed `é` as a composed one. Patterns are
    /// stored folded ([`fold_path`]); candidates are folded before matching.
    fold_case: bool,
}

#[derive(Clone, Debug)]
struct CompiledPattern {
    path: String,
    flavor: Flavor,
    directory_base: Option<String>,
    /// Whole segments before the first glob; a matching path must start here.
    literal_prefix: String,
}
impl CompiledPattern {
    /// This pattern as a case-folding host compares it.
    fn folded(self) -> Self {
        let path = fold_path(&self.path);
        Self {
            literal_prefix: literal_prefix(&path).into(),
            directory_base: self.directory_base.as_deref().map(fold_path),
            path,
            flavor: self.flavor,
        }
    }
    /// Upper bound on `glob_match` steps for one candidate, directory glob included.
    fn match_cost(&self, candidate: &str) -> usize {
        let pattern = self.path.chars().count()
            + self
                .directory_base
                .as_deref()
                .map_or(0, |base| base.chars().count());
        pattern.saturating_mul(candidate.chars().count())
    }
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

    /// The stricter of two policies' decisions for one tool call (drop, then
    /// metadata-only, then keep), `self` on a tie.
    #[must_use]
    pub fn stricter(self, other: Self) -> Self {
        let rank = |decision: &Self| match decision.protocol.disposition() {
            CaptureDisposition::Keep => 0,
            CaptureDisposition::MetadataOnly => 1,
            CaptureDisposition::Drop => 2,
        };
        if rank(&other) > rank(&self) {
            other
        } else {
            self
        }
    }
}

impl CapturePolicy {
    /// This policy's resolution state: whether a marker was absent, parsed
    /// into an active pattern set, or present but rejected (bad TOML, a
    /// `[capture]` shape `read_capture_config` doesn't accept, or a pattern
    /// `compile` rejects, e.g. an unsupported glob). `Invalid` already fails
    /// closed in [`Self::inspect`] — this accessor exists for diagnostics
    /// (`ai-memory doctor`) that want to say *why* without re-deriving it
    /// from scratch or trusting a parse-only check that misses the
    /// `compile`-rejected case.
    #[must_use]
    pub const fn state(&self) -> PolicyState {
        self.state
    }

    /// Resolves a marker config atomically. Empty parsed configuration is inactive.
    /// `home_dir` is required only when at least one pattern starts with `~/`.
    #[must_use]
    pub fn resolve(source: CaptureSource<'_>, marker_dir: &str, home_dir: Option<&str>) -> Self {
        Self::resolve_for_host(source, marker_dir, home_dir, cfg!(target_os = "macos"))
    }
    /// [`Self::resolve`] for a host whose filesystem does (`fold_case`) or
    /// does not ignore case. Only the client holds the patterns, so the
    /// server's re-inspection never depends on this.
    fn resolve_for_host(
        source: CaptureSource<'_>,
        marker_dir: &str,
        home_dir: Option<&str>,
        fold_case: bool,
    ) -> Self {
        let config = match source {
            CaptureSource::Absent => return Self::inactive(),
            CaptureSource::Invalid => {
                return Self {
                    state: PolicyState::Invalid,
                    patterns: Vec::new(),
                    home: None,
                    root: None,
                    fold_case,
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
                patterns: if fold_case {
                    patterns.into_iter().map(CompiledPattern::folded).collect()
                } else {
                    patterns
                },
                home: home_dir.and_then(|dir| normalize_root(dir).ok()),
                root: normalize_root(marker_dir).ok(),
                fold_case,
            },
            Err(_) => Self {
                state: PolicyState::Invalid,
                patterns: Vec::new(),
                home: None,
                root: None,
                fold_case,
            },
        }
    }
    fn inactive() -> Self {
        Self {
            state: PolicyState::Inactive,
            patterns: Vec::new(),
            home: None,
            root: None,
            fold_case: false,
        }
    }
    /// Evaluates every policy state using only direct fixture-backed schemas.
    #[must_use]
    pub fn inspect(&self, agent: AgentKind, raw: &Value, cwd: &str) -> CaptureDecision {
        let extracted = extract(agent, raw);
        let (disposition, extraction) = match self.state {
            PolicyState::Inactive => (CaptureDisposition::Keep, extracted.state),
            // A broken marker cannot prove a shell command's arguments, or a
            // search's matches, miss every ignored path, so it fails closed
            // like a file tool, readable or not.
            PolicyState::Invalid
                if extracted.family == ToolFamily::File
                    || extracted.family == ToolFamily::SearchList
                    || extracted.shell
                    || extracted.output_reader =>
            {
                (CaptureDisposition::MetadataOnly, extracted.state)
            }
            PolicyState::Invalid => (CaptureDisposition::Keep, extracted.state),
            // A tool that reads back an earlier command's output names no
            // path of its own, so nothing proves that command missed every
            // ignored path.
            PolicyState::Active if extracted.output_reader => {
                (CaptureDisposition::MetadataOnly, extracted.state)
            }
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
                    Some(paths) => {
                        let matched = if extracted.globs {
                            self.match_globs(paths, cwd)
                        } else {
                            self.match_paths(paths, cwd)
                        };
                        // A harness's saved copy of an earlier tool's output
                        // is read back like `BashOutput`; past the budget
                        // that check proves nothing either.
                        let spooled = || -> Result<bool, ()> {
                            let mut work = 0_usize;
                            for path in paths {
                                let readings = if extracted.globs {
                                    normalize_readings(path, cwd).ok().flatten()
                                } else {
                                    normalize_candidates(path, cwd)
                                };
                                for candidate in readings.into_iter().flatten() {
                                    if output_spool(&candidate.path, &mut work)? {
                                        return Ok(true);
                                    }
                                }
                            }
                            Ok(false)
                        };
                        match matched {
                            Ok(true) => (CaptureDisposition::Drop, ExtractionState::Extracted),
                            Ok(false) if spooled().unwrap_or(true) => {
                                (CaptureDisposition::MetadataOnly, ExtractionState::Extracted)
                            }
                            Ok(false) => (CaptureDisposition::Keep, ExtractionState::Extracted),
                            Err(()) => (
                                CaptureDisposition::MetadataOnly,
                                ExtractionState::MissingOrMalformed,
                            ),
                        }
                    }
                },
                // Program source builds its paths at run time, like
                // `python3 -c`, and a command longer than the matcher splits
                // is not read at all; neither proves a miss.
                ToolFamily::NonFile
                    if extracted.inline_code
                        || extracted
                            .command
                            .as_ref()
                            .is_some_and(ShellCommand::oversized) =>
                {
                    (CaptureDisposition::MetadataOnly, extracted.state)
                }
                // A shell command has no path field, but its arguments can
                // still name an ignored file whose content lands in the
                // output (`cat docs/adr/*.md`). An exhausted match budget
                // fails closed like an unprovable file candidate.
                ToolFamily::NonFile => match extracted.command.as_ref().map(|command| {
                    let base = command_base(extracted.workdir.as_deref(), cwd);
                    // A Windows shell started in a directory the matcher
                    // cannot place resolves every relative argument from it.
                    let unplaced = windows_session(cwd)
                        && extracted
                            .workdir
                            .as_deref()
                            .is_some_and(|dir| windows_unplaceable(dir, cwd));
                    // PowerShell's backquote and cmd's caret escape the next
                    // character, so their commands also get a reading
                    // without them.
                    let split =
                        command.split(extracted.powershell || flavor_of(cwd) == Flavor::Windows);
                    self.match_command(&split.words, &base)
                        .map(|(matched, spool)| (matched, split.opaque || spool || unplaced))
                }) {
                    Some(Ok((true, _)) | Err(())) => (CaptureDisposition::Drop, extracted.state),
                    // Syntax the matcher does not evaluate (brace expansion,
                    // bracket classes, zsh groups, encoded escapes) and inline
                    // interpreter code prove nothing either way, and a
                    // harness's saved output file holds an earlier command's
                    // output.
                    Some(Ok((false, true))) => (CaptureDisposition::MetadataOnly, extracted.state),
                    Some(Ok((false, false))) | None => (CaptureDisposition::Keep, extracted.state),
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
            .map(|path| normalize_candidates(path, cwd))
            .collect();
        let candidates = candidates.ok_or(())?.into_iter().flatten();
        let mut work = 0_usize;
        for candidate in candidates {
            let candidate = self.fold_candidate(candidate);
            for pattern in self
                .patterns
                .iter()
                .filter(|pattern| pattern.flavor == candidate.flavor)
            {
                work = work
                    .checked_add(pattern.match_cost(&candidate.path))
                    .ok_or(())?;
                if work > MAX_MATCH_WORK {
                    return Err(());
                }
                if glob_match(
                    &pattern.path,
                    &candidate.path,
                    pattern.directory_base.as_deref(),
                    pattern.flavor == Flavor::Windows || self.fold_case,
                ) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
    /// Lexically matches every path-like shell argument. Nothing is expanded
    /// or executed: variables, command substitution, and `cd` are not
    /// followed, so this narrows the leak rather than closing every alias.
    /// Also reports whether an argument names a harness's saved tool output
    /// ([`output_spool`]) or, in a Windows session, a file the matcher cannot
    /// place ([`windows_unplaceable`]); neither proves a miss.
    fn match_command(&self, words: &[(String, bool)], cwd: &str) -> Result<(bool, bool), ()> {
        let mut work = 0_usize;
        let mut spool = false;
        let windows = windows_session(cwd);
        // A POSIX shell's reading of a native path (`C:\repo\x` read as
        // `C:repox`) is drive-relative too; it proves nothing while the
        // literal spelling it came from is placeable. Only doubled
        // backslashes, which no native spelling of a placeable path holds,
        // read as a device path, so that reading is never excused.
        let mut drive_relative = Vec::new();
        let mut placed = std::collections::HashSet::new();
        for (word, tilde) in words {
            for (argument, from_root) in shell_arguments(word) {
                let expanded = expand_tilde(argument, *tilde, self.home.as_deref(), cwd);
                if windows {
                    if !windows_unplaceable(&expanded, cwd) {
                        if expanded.contains('\\') {
                            placed.insert(without_backslash_escapes(&expanded));
                        }
                    } else if valid_drive_prefix(strip_powershell_provider(&expanded)) {
                        drive_relative.push(expanded.clone());
                    } else {
                        spool = true;
                    }
                }
                // Git reads `REV:path` from the repository root, which only
                // the marker directory approximates; try the cwd as well.
                let root = (from_root && !is_absolute(&expanded))
                    .then_some(self.root.as_deref())
                    .flatten();
                for base in std::iter::once(cwd).chain(root) {
                    // Not a usable path (drive-relative, a device namespace,
                    // oversized, bad cwd).
                    let Some(readings) = normalize_readings(&expanded, base)? else {
                        continue;
                    };
                    for candidate in readings {
                        spool |= output_spool(&candidate.path, &mut work)?;
                        if self.match_pattern_candidate(&candidate, false, &mut work)? {
                            return Ok((true, spool));
                        }
                    }
                }
            }
        }
        spool |= drive_relative.iter().any(|word| !placed.contains(word));
        Ok((false, spool))
    }
    /// `candidate` as this host compares it.
    fn fold_candidate(&self, candidate: Normalized) -> Normalized {
        if self.fold_case {
            Normalized {
                path: fold_path(&candidate.path),
                flavor: candidate.flavor,
            }
        } else {
            candidate
        }
    }
    /// Matches the path arguments of Gemini CLI's `read_many_files`, which are
    /// globs expanded case-insensitively (`nocase`) with `**` recursing into
    /// any depth. A literal path matches like a file tool's; a glob drops when
    /// it could expand into an ignored path, the same over-approximation
    /// shell globs get. Glob syntax the matcher cannot evaluate (classes,
    /// braces, extglobs, negation) fails closed.
    fn match_globs(&self, paths: &[String], cwd: &str) -> Result<bool, ()> {
        let mut work = 0_usize;
        for path in paths {
            if path.contains(['[', ']', '{', '}', '(', ')', '!']) {
                return Err(());
            }
            for candidate in normalize_readings(path, cwd)?.ok_or(())? {
                if self.match_pattern_candidate(&candidate, true, &mut work)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
    /// Whether one normalized candidate, literal or glob, names or can expand
    /// into an ignored path. `tool_glob` selects `read_many_files` expansion
    /// (case-insensitive, recursive `**`) over a shell's; Windows, and a
    /// case-folding host, always ignore case.
    fn match_pattern_candidate(
        &self,
        candidate: &Normalized,
        tool_glob: bool,
        work: &mut usize,
    ) -> Result<bool, ()> {
        let folded;
        let candidate = if self.fold_case {
            folded = self.fold_candidate(candidate.clone());
            &folded
        } else {
            candidate
        };
        let insensitive = tool_glob || candidate.flavor == Flavor::Windows || self.fold_case;
        let is_glob = candidate.path.contains(['*', '?']);
        // `read_many_files` reads a directory entry (`docs`, `.`) as
        // `entry/**`, and nothing here can tell a directory from a file.
        let directory = tool_glob && !is_glob;
        for pattern in self
            .patterns
            .iter()
            .filter(|pattern| pattern.flavor == candidate.flavor)
        {
            // Linear and bounded by the candidate itself; only the
            // quadratic matches below are charged to the budget.
            let under_prefix =
                starts_with_chars(&candidate.path, &pattern.literal_prefix, insensitive);
            if !under_prefix && !is_glob && !directory {
                continue;
            }
            *work = work
                .checked_add(pattern.match_cost(&candidate.path))
                .and_then(|work| {
                    work.checked_add(if directory {
                        pattern.match_cost(&candidate.path)
                    } else {
                        0
                    })
                })
                .ok_or(())?;
            if *work > MAX_MATCH_WORK {
                return Err(());
            }
            // Treat `**` as recursive for shell globs too: bash leaves it a
            // single segment, but zsh (and bash with `globstar`) recurse, and
            // an over-eager drop is the safe side of a capture exclusion.
            let reaches = globstar_reaches;
            if (under_prefix
                && glob_match(
                    &pattern.path,
                    &candidate.path,
                    pattern.directory_base.as_deref(),
                    insensitive,
                ))
                || (is_glob && reaches(&candidate.path, &pattern.literal_prefix, insensitive))
                || (directory && glob_matches_below(&pattern.path, &candidate.path, insensitive))
            {
                return Ok(true);
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

/// Rebuilds the payload shape [`CapturePolicy::inspect`] and
/// [`absolute_tool_location_paths`] read for `agent` from a tool call recorded
/// outside a hook — a native transcript event replayed by `backfill` or the
/// `ai-memory run` ledger — so the import applies the live decision table
/// rather than a second matcher. `args: None` means the arguments could not be
/// recovered, which a file tool treats as malformed.
///
/// A transcript can name a tool differently from the hook: Codex records its
/// shell tool as `exec_command` with `{cmd, workdir}`, which reaches hooks as
/// `Bash`. Such a call is rebuilt under the hook's name and fields, so the
/// import classifies it as the live hook does.
#[must_use]
pub fn tool_call_payload(
    agent: AgentKind,
    name: &str,
    args: Option<&Value>,
    call_id: Option<&str>,
) -> Value {
    let args = args.cloned().unwrap_or(Value::Null);
    let (name, args) = hook_tool_call(agent, name, args);
    let mut payload = match agent {
        AgentKind::AntigravityCli => json!({"toolCall": {"name": name, "args": args}}),
        AgentKind::Zero => json!({"toolName": name, "input": args}),
        AgentKind::OpenCode | AgentKind::Omp | AgentKind::Pi | AgentKind::OpenClaw => {
            json!({"tool": name, "args": args})
        }
        _ => json!({"tool_name": name, "tool_input": args}),
    };
    if let (Some(id), Some(object)) = (call_id, payload.as_object_mut()) {
        object.insert("tool_use_id".into(), Value::String(id.into()));
    }
    payload
}

/// The name and arguments the live hook receives for a call a transcript
/// records under its own name. Unreadable arguments stay unreadable.
fn hook_tool_call(agent: AgentKind, name: &str, args: Value) -> (&str, Value) {
    if agent == AgentKind::Codex && name.eq_ignore_ascii_case("exec_command") {
        let Value::Object(recorded) = args else {
            return ("Bash", args);
        };
        let mut input = Map::new();
        for (from, to) in [("cmd", "command"), ("workdir", "workdir")] {
            if let Some(value) = recorded.get(from) {
                input.insert(to.into(), value.clone());
            }
        }
        return ("Bash", Value::Object(input));
    }
    (name, args)
}

#[derive(Default)]
struct Extracted<'a> {
    family: ToolFamily,
    paths: Option<Vec<String>>,
    /// A shell tool's command, not yet split into words.
    command: Option<ShellCommand<'a>>,
    /// A command-running tool, even when its command is unreadable.
    /// `web_search` is non-file but runs nothing.
    shell: bool,
    workdir: Option<String>,
    /// A search/list tool's explicit directory or file argument. Used only to
    /// locate where the tool ran; capture policy drops search output under an
    /// active marker regardless of it.
    search_paths: Option<Vec<String>>,
    /// Every place a search/list tool reads from, for the cross-checkout
    /// lookup: its directory and file arguments (`roots`) and the glob
    /// arguments that select below them (`patterns`). `None` when one of
    /// them is not a string, so the lookup cannot be made.
    search_scope: Option<SearchScope>,
    /// The files an `apply_patch` call's patch text names, for the
    /// cross-checkout lookup only; the call's own decision does not read
    /// them.
    patch_paths: Option<Vec<String>>,
    /// The paths are globs (Gemini CLI `read_many_files`), not literal files.
    globs: bool,
    /// The tool reads back output of an earlier command (`BashOutput`).
    output_reader: bool,
    /// Claude Code's `PowerShell` tool.
    powershell: bool,
    /// A non-file tool whose input is program source ([`INLINE_CODE_TOOL_NAMES`]).
    inline_code: bool,
    call_id: Option<String>,
    state: ExtractionState,
}

fn extract(agent: AgentKind, raw: &Value) -> Extracted<'_> {
    let Some(object) = raw.as_object() else {
        return Extracted::default();
    };
    let extracted = match agent {
        AgentKind::AntigravityCli => object
            .get("toolCall")
            .and_then(Value::as_object)
            .and_then(|call| Some((call.get("name")?.as_str()?, call.get("args")))),
        AgentKind::Cline => object
            .get("tool_call")
            .or_else(|| object.get("tool_result"))
            .and_then(Value::as_object)
            .and_then(|call| Some((call.get("name")?.as_str()?, call.get("input")))),
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
        | AgentKind::QwenCode
        | AgentKind::Qoder
        | AgentKind::Devin
        | AgentKind::Hermes
        | AgentKind::KiroCli
        | AgentKind::Pool
        // ZCode mirrors Claude Code's snake_case `tool_name`/`tool_input`
        // aliases on every tool event (live-captured, #512).
        // GrizzyBot posts the same Claude Code-shaped payload from its agent
        // loop.
        | AgentKind::Grizzybot
        | AgentKind::Zcode
        // GitHub Copilot CLI, PascalCase-configured for the Claude-compatible
        // payload shape (#1040).
        | AgentKind::CopilotCli
        // Factory Droid's hooks send the same Claude Code shape (#1240).
        | AgentKind::Droid
        // Meta Muse Code too, with its own tool names (Muse Code 1.4.7).
        // CodeBuddy Code uses Claude Code's names and the same two fields
        // (#1299), so `[capture] ignore_paths` classifies them as Claude's.
        | AgentKind::Muse
        | AgentKind::CodeBuddy
        | AgentKind::IbmBob
        // DSH's Claude Code hooks bridge forwards the same `tool_name` /
        // `tool_input` pair, so its file and shell calls read the ignore
        // paths exactly as Claude Code's do.
        | AgentKind::Dsh => object
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
    let family = agent_family(agent, name);
    let paths = (family == ToolFamily::File)
        .then(|| {
            args.and_then(|value| {
                if agent == AgentKind::Muse
                    && MUSE_MEMORY_TOOL_NAMES.contains(&name.to_ascii_lowercase().as_str())
                {
                    muse_memory_paths(value)
                } else {
                    extract_paths(name, value)
                }
            })
        })
        .flatten();
    let command = (family == ToolFamily::NonFile)
        .then(|| args.and_then(shell_command))
        .flatten();
    let search_paths = (family == ToolFamily::SearchList)
        .then(|| args.and_then(|value| search_direct_paths(agent, value.as_object()?)))
        .flatten();
    let search_scope = (family == ToolFamily::SearchList)
        .then(|| search_scope(agent, name, args))
        .flatten();
    // Droid's `ApplyPatch` takes the same envelope in `input` as Codex's
    // `apply_patch`.
    let patch_name = if agent == AgentKind::Droid && name.eq_ignore_ascii_case("applypatch") {
        "apply_patch"
    } else {
        name
    };
    let patch_paths = (family == ToolFamily::File && paths.is_none())
        .then(|| args.and_then(|value| patch_paths(patch_name, value)))
        .flatten();
    // OpenCode `bash`, OpenClaw `exec` and Codex `shell` run in `workdir`
    // when given, Gemini CLI `run_shell_command` in `dir_path` (`directory`
    // before it was renamed), and Antigravity `run_command` in `Cwd`, so
    // relative arguments resolve from there.
    let shell = family == ToolFamily::NonFile && !name.eq_ignore_ascii_case("web_search");
    let workdir_keys: &[&str] = if agent == AgentKind::AntigravityCli {
        &["Cwd"]
    } else {
        &["workdir", "dir_path", "directory"]
    };
    let workdir = command
        .as_ref()
        .and_then(|_| {
            let args = args?;
            workdir_keys.iter().find_map(|key| args.get(*key))?.as_str()
        })
        .filter(|dir| !dir.trim().is_empty())
        .map(str::to_owned);
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
        command,
        shell,
        workdir,
        search_paths,
        search_scope,
        patch_paths,
        globs: name.eq_ignore_ascii_case("read_many_files"),
        // Muse's `bash_input` writes to a shell an earlier `bash` call
        // started and returns what it printed, like Codex `write_stdin`.
        output_reader: reads_earlier_output(name)
            || (agent == AgentKind::Muse && name.eq_ignore_ascii_case("bash_input")),
        // Claude Code's `PowerShell` and DSH's Windows `pwsh` share the
        // backquote escaping the command splitter has to model.
        powershell: name.eq_ignore_ascii_case("powershell") || name.eq_ignore_ascii_case("pwsh"),
        inline_code: family == ToolFamily::NonFile
            && INLINE_CODE_TOOL_NAMES.contains(&name.to_ascii_lowercase().as_str()),
        call_id,
        state,
    }
}

/// [`family`] plus tool names only one agent is documented to use, so a
/// same-named tool of another agent keeps its current classification.
fn agent_family(agent: AgentKind, name: &str) -> ToolFamily {
    if agent == AgentKind::CopilotCli {
        // GitHub Copilot CLI's runtime tool names: `view` reads a file or
        // directory, `create` writes one, `str_replace_editor` edits, each
        // naming its target in `path`; `rg` is its `grep` alias.
        match name.to_ascii_lowercase().as_str() {
            "view" | "create" | "str_replace_editor" => return ToolFamily::File,
            "rg" => return ToolFamily::SearchList,
            _ => {}
        }
    }
    if agent == AgentKind::Muse {
        // Meta Muse Code's memory tools name a markdown file in `path`
        // (resolved by [`muse_memory_paths`]) and `monitor` runs a `command`
        // and streams its output (Muse Code 1.4.7). `bash_input`, which
        // drives a running `bash`, stays unknown like the other output
        // readers. `read_file`, `write_file`, `edit_file`,
        // `search`, `bash`, `web_fetch` and `web_search` carry shared names.
        let lower = name.to_ascii_lowercase();
        if MUSE_MEMORY_TOOL_NAMES.contains(&lower.as_str()) {
            return ToolFamily::File;
        }
        if lower == "monitor" {
            return ToolFamily::NonFile;
        }
    }
    if agent == AgentKind::Droid {
        // Factory Droid writes a new file with `Create` and applies a
        // Codex-style patch envelope with `ApplyPatch` (0.240.0 tool
        // schemas, #1240). Its `Read`, `Edit`, `LS`, `Grep`, `Glob` and
        // `Execute` already carry shared names.
        match name.to_ascii_lowercase().as_str() {
            "create" | "applypatch" => return ToolFamily::File,
            _ => {}
        }
    }
    family(name)
}

/// Whether a tool returns output of a command started by an earlier call
/// ([`OUTPUT_READER_TOOL_NAMES`]). That command's own event may have been
/// dropped, so its output must not come back through these.
fn reads_earlier_output(name: &str) -> bool {
    OUTPUT_READER_TOOL_NAMES.contains(&name.to_ascii_lowercase().as_str())
}

/// Whether a normalized path is a file a harness saved a tool's output to and
/// tells the model to read back: Claude Code's large results
/// (`~/.claude/projects/<project>/<session>/tool-results/<id>.txt`) and
/// background task output (`<tmp>/claude-<uid>/<project>/<session>/tasks/<id>.output`),
/// OpenCode's truncated output (`<data>/opencode/tool-output/tool_<id>`) and
/// Pi's (`<tmp>/pi-bash-<id>.log`). The command that produced it may have been
/// dropped, so, like [`reads_earlier_output`], nothing proves it missed every
/// ignored path. Segment names compare without regard to ASCII case.
///
/// A path holding `*` or `?` is a glob that could expand to such a file
/// (`~/.claude/projects/*/*/tool-results/*`): each of its segments is read as
/// a one-segment glob (`**` included) that fits a named directory or file
/// when the two globs share a match. At least one segment that fits must
/// spell part of what makes its name distinctive (`claude`, `projects`,
/// `tool-results`, `tasks`, `.output`, `opencode`, `tool-output`, `pi-bash-`)
/// in literal text, so a bare `*` or `*/*`, or a `*.log` whose wildcard stands
/// where `pi-bash-` would be, is never taken for a saved output file.
///
/// Each segment is fitted against each shape at most once, so the work is
/// linear in the path; it is charged to the caller's match budget (`work`,
/// up to [`MAX_MATCH_WORK`]), and past it nothing is proven (`Err`).
fn output_spool(path: &str, work: &mut usize) -> Result<bool, ()> {
    let segments: Vec<Vec<char>> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.to_ascii_lowercase().chars().collect())
        .collect();
    let Some((file, directories)) = segments.split_last() else {
        return Ok(false);
    };
    let mut charge = |segment: &[char], shape: &str| -> Result<(), ()> {
        *work = work
            .checked_add((segment.len() + 1).saturating_mul(shape.chars().count() + 1))
            .ok_or(())?;
        if *work > MAX_MATCH_WORK {
            Err(())
        } else {
            Ok(())
        }
    };
    let mut fits = |segment: &[char], shape: &str| -> Result<bool, ()> {
        charge(segment, shape)?;
        Ok(globs_intersect(segment, &shape.chars().collect::<Vec<_>>()))
    };
    let count = directories.len();
    // `<..claude..>/projects/../tool-results/..`: whether some directory past
    // `index` fits `tool-results`, and whether one that fits also spells it,
    // each computed once for every `index` (`after[index]`).
    let mut after: Option<Vec<(bool, bool)>> = None;
    let mut claude_results = false;
    for index in 1..count {
        let (claude, projects) = (&directories[index - 1], &directories[index]);
        if !fits(claude, "*claude*")? || !fits(projects, "projects")? {
            continue;
        }
        if after.is_none() {
            let mut suffix = vec![(false, false); count];
            let mut seen = (false, false);
            for at in (0..count).rev() {
                suffix[at] = seen;
                let results = &directories[at];
                if fits(results, "tool-results")? {
                    seen = (true, seen.1 || spells(results, "tool-results"));
                }
            }
            after = Some(suffix);
        }
        let (fitting, spelled) = after.as_ref().map_or((false, false), |after| after[index]);
        if spelled || (fitting && (spells(claude, "claude") || spells(projects, "projects"))) {
            claude_results = true;
            break;
        }
    }
    let claude_task = count > 0 && {
        let parent = &directories[count - 1];
        fits(parent, "tasks")? && fits(file, "*.output")? && {
            let named = spells(parent, "tasks") || spells(file, ".output");
            let mut found = false;
            for claude in directories {
                if fits(claude, "claude*")? && (named || spells(claude, "claude")) {
                    found = true;
                    break;
                }
            }
            found
        }
    };
    let opencode = count >= 2 && {
        let (data, output) = (&directories[count - 2], &directories[count - 1]);
        fits(data, "opencode")?
            && fits(output, "tool-output")?
            && (spells(data, "opencode") || spells(output, "tool-output"))
    };
    let pi = fits(file, "pi-bash-*.log")? && spells(file, "pi-bash-");
    Ok(claude_results || claude_task || opencode || pi)
}

/// Whether a one-segment glob spells part of `word` in literal text: some
/// run of it between wildcards shares at least three characters in a row
/// with `word` (all of a shorter one). `tool-res*` and `pi-*` do; `*.log`,
/// `t*` and `*` do not.
fn spells(segment: &[char], word: &str) -> bool {
    let word: Vec<char> = word.chars().collect();
    let needed = word.len().min(3);
    segment.split(|c| matches!(c, '*' | '?')).any(|run| {
        run.windows(needed)
            .any(|piece| word.windows(needed).any(|w| w == piece))
    })
}

/// Whether two one-segment globs over `*` and `?` match a common string.
/// A literal is a glob without wildcards, so for one this is `glob_match`.
fn globs_intersect(left: &[char], right: &[char]) -> bool {
    // `common[i][j]`: some string matches both `left[i..]` and `right[j..]`.
    let mut common = vec![vec![false; right.len() + 1]; left.len() + 1];
    common[left.len()][right.len()] = true;
    for i in (0..=left.len()).rev() {
        for j in (0..=right.len()).rev() {
            if i == left.len() && j == right.len() {
                continue;
            }
            common[i][j] = if left.get(i) == Some(&'*') {
                common[i + 1][j] || (j < right.len() && common[i][j + 1])
            } else if right.get(j) == Some(&'*') {
                common[i][j + 1] || (i < left.len() && common[i + 1][j])
            } else if let (Some(l), Some(r)) = (left.get(i), right.get(j)) {
                (*l == '?' || *r == '?' || l == r) && common[i + 1][j + 1]
            } else {
                false
            };
        }
    }
    common[0][0]
}

/// Tool names (compared ASCII-lowercased) whose calls name the files they
/// read or write. The generated TypeScript integrations embed this list, so
/// the two front doors cannot drift apart. Gemini CLI's `read_many_files`
/// reads every file its `include` globs expand to.
pub const FILE_TOOL_NAMES: &[&str] = &[
    "read",
    "write",
    "edit",
    "apply_patch",
    "notebookedit",
    "notebook_edit",
    "create_file",
    "delete_file",
    "remove",
    "rename_file",
    "move_file",
    "multi_edit",
    "multiedit",
    "replace",
    "replace_all",
    "view_file",
    "replace_file_content",
    "multi_replace_file_content",
    "write_to_file",
    "fs_read",
    "fs_write",
    "read_file",
    "write_file",
    "edit_file",
    "patch",
    "read_many_files",
];

/// Search and list tools, whose output under an active policy is dropped.
/// Includes Gemini CLI's directory listing and the pre-rename name of its
/// `grep_search` that it still accepts as an alias.
pub const SEARCH_LIST_TOOL_NAMES: &[&str] = &[
    "search",
    "grep",
    "glob",
    "find",
    "list",
    "ls",
    "list_files",
    "read_dir",
    "list_dir",
    "grep_search",
    "search_files",
    "find_by_name",
    "list_directory",
    "search_file_content",
];

/// Tools that name no workspace file path of their own: shells (Claude Code's
/// `PowerShell` beside `Bash` with the same `tool_input.command` field, #1186;
/// Gemini CLI's `run_shell_command`, `{command, dir_path?}`) and the Hermes
/// Agent tools that execute code, drive a browser, or fetch the web
/// (agent/shell_hooks.py envelope; names from the model_tools registry,
/// live-captured 2026-10-07). DeepSeek Harness reaches the same shell through
/// the win32 layer: its POSIX tool is `bash` and its Windows tool is `pwsh`
/// (`@deepseek-ai/dsh-tool-pwsh`), so both names must classify or an active
/// `ignore_paths` never reads the command's arguments. They are non-file
/// rather than unknown because family `unknown` drops the tool output from
/// observation bodies.
pub const NON_FILE_TOOL_NAMES: &[&str] = &[
    "bash",
    "shell",
    "shell_command",
    "exec",
    "execute",
    "run_command",
    "web_search",
    "search_web",
    "manage_task",
    "manage_subagents",
    "terminal",
    "execute_bash",
    "execute_cmd",
    "powershell",
    "pwsh",
    "run_shell_command",
    "execute_code",
    "run_code",
    "browser_exec",
    "browser_navigate",
    "web_extract",
    "web_fetch",
    "delegate_task",
];

/// Non-file tools whose input is program source (Hermes `execute_code`,
/// `{"code": "..."}`, and DSH's reserved `run_code` Code Mode transport,
/// which takes the same `code` field —
/// <https://github.com/deepseek-ai/deepseek-harness/tree/main/packages/core/tools>):
/// like `python3 -c`, the program builds its paths at run time, so nothing
/// lexical proves it read no ignored file. DSH's `headless` profile mounts
/// Code Mode, so `run_code` is the tool its one-shot runs call.
pub const INLINE_CODE_TOOL_NAMES: &[&str] = &["execute_code", "run_code"];

/// Tools that return output of a command started by an earlier call: Claude
/// Code `BashOutput`/`TaskOutput`, Codex `write_stdin`, Hermes and OpenClaw
/// `process`, Copilot CLI `read_bash`/`write_bash` and their PowerShell twins.
pub const OUTPUT_READER_TOOL_NAMES: &[&str] = &[
    "bashoutput",
    "taskoutput",
    "write_stdin",
    "process",
    "read_bash",
    "write_bash",
    "read_powershell",
    "write_powershell",
];

fn family(name: &str) -> ToolFamily {
    let name = name.to_ascii_lowercase();
    let name = name.as_str();
    if FILE_TOOL_NAMES.contains(&name) {
        ToolFamily::File
    } else if SEARCH_LIST_TOOL_NAMES.contains(&name) {
        ToolFamily::SearchList
    } else if NON_FILE_TOOL_NAMES.contains(&name) {
        ToolFamily::NonFile
    } else {
        ToolFamily::Unknown
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

/// Whether a native tool call id may be stored: the only call id the server
/// keeps beside a metadata-only tool observation.
#[must_use]
pub fn valid_call_id(id: &str) -> bool {
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
    // Gemini CLI `read_many_files` names its globs in `include` (`paths`,
    // which `direct_paths` reads, before a rename). `exclude` only narrows
    // the read, so it is not a candidate.
    if name.eq_ignore_ascii_case("read_many_files")
        && let Some(include) = object.get("include")
    {
        let entries = include.as_array()?;
        if paths.len().checked_add(entries.len())? > MAX_CAPTURE_CANDIDATES {
            return None;
        }
        for entry in entries {
            paths.push(entry.as_str()?.to_owned());
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

/// Return the bounded, normalized absolute paths that locate where a
/// recognized tool call ran, in the path's own separator style: a file tool's
/// targets, a search/list tool's explicit directory, or a shell command's
/// `workdir`.
///
/// Native hook clients use this only to notice a tool call that operates in
/// another checkout while the harness keeps reporting a different cwd — a
/// subagent's parent session (#932), or Hermes' process cwd (#1199).
/// Relative paths are intentionally excluded: joining them to any cwd other
/// than the payload's would be ambiguous, and they cannot independently prove
/// a cross-project target. Free-form command text is never parsed for a
/// location; only the structured `workdir` field counts. Unknown schemas and
/// mixed/oversized inputs fail closed with `None`, matching capture-policy
/// extraction.
#[must_use]
pub fn absolute_tool_location_paths(
    agent: AgentKind,
    raw: &Value,
    cwd: &str,
) -> Option<Vec<String>> {
    let extracted = extract(agent, raw);
    let paths = match extracted.family {
        ToolFamily::File => extracted.paths?,
        ToolFamily::SearchList => extracted.search_paths?,
        ToolFamily::NonFile => vec![extracted.workdir?],
        ToolFamily::Unknown => return None,
    };
    paths
        .iter()
        .map(|path| {
            is_absolute(path)
                .then(|| normalize_candidate(path, cwd))
                .flatten()
                .map(native_path)
        })
        .collect()
}

/// A normalized candidate in its own separator style. Matching works on
/// `/`-separated paths, but this path leaves the policy module: the hook
/// sends a directory as the event's `cwd` and looks markers up from it. It
/// keeps the native style (`C:\…`, like every other cwd the hook forwards)
/// so the server never sees two spellings of one checkout; segment
/// normalization (`.`/`..`) still applies.
fn native_path(candidate: Normalized) -> String {
    match candidate.flavor {
        Flavor::Windows => candidate.path.replace('/', "\\"),
        Flavor::Posix => candidate.path,
    }
}

/// Upper bound on the locations [`capture_lookup_locations`] returns for one
/// tool call.
pub const MAX_LOOKUP_LOCATIONS: usize = 4_096;

/// The absolute locations a recognized file, search/list or shell tool call
/// names, in their own separator style, for a hook client to find the marker
/// that governs each: every file-tool target (for `apply_patch`, the files its
/// patch names), every directory, file and path-like glob argument of a
/// search/list tool, and every shell argument that names a path (one with a
/// separator, after `~/` expansion) resolved from the tool's `workdir` or the
/// `cwd`, plus that directory itself.
///
/// A checkout's `ignore_paths` protect its files wherever the call is made
/// from, so a client applies the policy of each location's own marker as
/// well as the event's (`cat ../other-checkout/secret.env`). A location may
/// hold glob syntax; [`glob_walk`] says how to follow it. Unknown tools yield
/// nothing; their policy is the event's. A glob whose `..` follows a `**`
/// yields each of its readings (`normalize_readings`), and a shell script
/// with a zsh glob group or ANSI-C numeric escapes also yields its words as
/// the shell expands them (`ShellWords::expanded`). `None` when nothing
/// proves which markers govern the call: a file tool whose targets cannot be
/// read or resolved, a search/list argument that is not a string, a glob with
/// more than 16 readings, or more than
/// [`MAX_LOOKUP_LOCATIONS`] distinct locations.
pub fn capture_lookup_locations(
    agent: AgentKind,
    raw: &Value,
    cwd: &str,
    home: Option<&str>,
) -> Option<Vec<String>> {
    let extracted = extract(agent, raw);
    let mut seen = std::collections::HashSet::new();
    let mut locations = Vec::new();
    let mut push = |candidate: Option<Normalized>| -> Option<()> {
        let Some(candidate) = candidate else {
            return Some(());
        };
        let location = native_path(candidate);
        if seen.insert(location.clone()) {
            if locations.len() == MAX_LOOKUP_LOCATIONS {
                return None;
            }
            locations.push(location);
        }
        Some(())
    };
    // Every reading of a glob whose `..` follows a `**` ([`normalize_readings`]).
    let readings = |path: &str, base: &str| normalize_readings(path, base).ok();
    match (extracted.family, extracted.command.as_ref()) {
        (ToolFamily::File, _) => {
            let paths = extracted
                .paths
                .as_ref()
                .or(extracted.patch_paths.as_ref())?;
            for path in paths {
                for candidate in readings(path, cwd)?? {
                    push(Some(candidate))?;
                }
            }
        }
        (ToolFamily::SearchList, _) => {
            let scope = extracted.search_scope.as_ref()?;
            let mut roots = Vec::new();
            for root in &scope.roots {
                for root in readings(root, cwd)?? {
                    roots.push(native_path(root.clone()));
                    push(Some(root))?;
                }
            }
            if roots.is_empty() {
                roots.push(cwd.to_owned());
            }
            // A glob argument without a separator only selects names below
            // its root, which the root's own lookup already covers.
            for pattern in scope.patterns.iter().filter(|p| p.contains(['/', '\\'])) {
                for root in &roots {
                    for candidate in readings(pattern, root)?? {
                        push(Some(candidate))?;
                    }
                }
            }
        }
        (ToolFamily::NonFile, Some(command)) => {
            if command.oversized() {
                return None;
            }
            let base = command_base(extracted.workdir.as_deref(), cwd);
            push(normalize_candidate(&base, &base))?;
            let home = home.and_then(|dir| normalize_root(dir).ok());
            let split = command.split(extracted.powershell || flavor_of(cwd) == Flavor::Windows);
            for (word, tilde) in split.words.iter().chain(&split.expanded) {
                for (argument, _) in shell_arguments(word) {
                    let expanded = expand_tilde(argument, *tilde, home.as_deref(), &base);
                    if expanded.contains(['/', '\\']) {
                        for candidate in readings(&expanded, &base)?.into_iter().flatten() {
                            push(Some(candidate))?;
                        }
                    }
                }
            }
        }
        _ => {}
    }
    Some(locations)
}

/// Where a search/list tool reads: see [`Extracted::search_scope`].
#[derive(Default)]
struct SearchScope {
    roots: Vec<String>,
    patterns: Vec<String>,
}

/// A search/list tool's directory and file arguments, and its glob
/// arguments: a glob tool's `pattern` (Claude Code `Glob`, Gemini CLI `glob`,
/// Antigravity `find_by_name`), and the file filters of a content search
/// (`glob`, `include`, `file_pattern`, Hermes `file_glob`, `Includes`). A
/// grep `pattern` is a regular expression, not a path, and is left out.
/// `None` when one of them is present but not a string (or, for a filter, a
/// list of strings).
fn search_scope(agent: AgentKind, name: &str, args: Option<&Value>) -> Option<SearchScope> {
    let object = match args {
        None | Some(Value::Null) => return Some(SearchScope::default()),
        Some(value) => value.as_object()?,
    };
    let mut scope = SearchScope::default();
    if object
        .keys()
        .any(|key| DIRECT_PATH_KEYS.contains(&key.as_str()) || key == "paths")
    {
        scope.roots = direct_paths(object)?;
    }
    for key in [
        "dir_path",
        "directory",
        "DirectoryPath",
        "SearchDirectory",
        "SearchPath",
    ] {
        if let Some(value) = object.get(key) {
            scope.roots.push(value.as_str()?.to_owned());
        }
    }
    if agent == AgentKind::Droid {
        for key in DROID_SEARCH_ROOT_KEYS {
            if let Some(value) = object.get(key) {
                scope.roots.push(value.as_str()?.to_owned());
            }
        }
    }
    let glob_tool = matches!(
        name.to_ascii_lowercase().as_str(),
        "glob" | "find" | "find_by_name"
    );
    let pattern_keys: &[&str] = if glob_tool {
        &[
            "pattern",
            "Pattern",
            "glob",
            "include",
            "file_pattern",
            "file_glob",
            "Includes",
        ]
    } else {
        &["glob", "include", "file_pattern", "file_glob", "Includes"]
    };
    // Droid's `Glob` takes `patterns` (a string or a list) and its `Grep`
    // filters files with `glob_pattern`; `excludePatterns` only narrows.
    let droid_pattern_keys: &[&str] = match agent {
        AgentKind::Droid if glob_tool => &["patterns", "glob_pattern"],
        AgentKind::Droid => &["glob_pattern"],
        _ => &[],
    };
    for key in pattern_keys.iter().chain(droid_pattern_keys) {
        match object.get(*key) {
            None => {}
            Some(Value::String(pattern)) => scope.patterns.push(pattern.clone()),
            Some(Value::Array(patterns)) => {
                for pattern in patterns {
                    scope.patterns.push(pattern.as_str()?.to_owned());
                }
            }
            Some(_) => return None,
        }
    }
    let total = scope.roots.len().checked_add(scope.patterns.len())?;
    (total <= MAX_CAPTURE_CANDIDATES
        && scope.roots.iter().chain(&scope.patterns).all(|path| {
            !path.trim().is_empty() && path.chars().count() <= MAX_CANDIDATE_PATH_CHARS
        }))
    .then_some(scope)
}

/// The files an `apply_patch` patch names in its `*** Add File:`,
/// `*** Update File:`, `*** Delete File:` and `*** Move to:` headers, from
/// the patch text itself or the `command`, `patch`, `patchText` or `input`
/// field that holds it. Codex trims each line before it reads a header, so
/// leading whitespace does not hide one. `None` when no header names a file.
fn patch_paths(name: &str, args: &Value) -> Option<Vec<String>> {
    if !matches!(name.to_ascii_lowercase().as_str(), "apply_patch" | "patch") {
        return None;
    }
    let text = match args {
        Value::String(text) => text.as_str(),
        Value::Object(object) => ["command", "patch", "patchText", "input"]
            .iter()
            .find_map(|key| object.get(*key)?.as_str())?,
        _ => return None,
    };
    let mut paths = Vec::new();
    for line in text.lines() {
        let Some(path) = [
            "*** Add File:",
            "*** Update File:",
            "*** Delete File:",
            "*** Move to:",
        ]
        .iter()
        .find_map(|header| line.trim_start().strip_prefix(header)) else {
            continue;
        };
        let path = path.trim();
        if path.is_empty() || path.chars().count() > MAX_CANDIDATE_PATH_CHARS {
            return None;
        }
        if paths.len() == MAX_CAPTURE_CANDIDATES {
            return None;
        }
        paths.push(path.to_owned());
    }
    (!paths.is_empty()).then_some(paths)
}

/// How a hook client reaches every directory a [`capture_lookup_locations`]
/// location could name when it holds glob syntax (`*`, `?`, a `[` class, a
/// `{` brace, a `(` group).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GlobWalk {
    /// No glob syntax: look the location up as a path.
    Literal,
    /// Look `root` (the leading segments without glob syntax, in the
    /// location's own separator style) up as a path, then follow `steps`
    /// through the directories below it.
    Walk {
        /// The deepest directory the glob cannot leave.
        root: String,
        /// One step per remaining segment; [`GlobStep::AnyDepth`] ends it.
        steps: Vec<GlobStep>,
    },
    /// Syntax a walk cannot follow: a brace or group that spans a separator,
    /// or an alternative of `..` or `~…` inside one (also one a brace
    /// expansion builds, or a directory alternative older bash expands to
    /// `..`), which leaves the root. Nothing proves which markers govern it.
    Unprovable,
}

/// One segment of a [`GlobWalk::Walk`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GlobStep {
    /// A segment without glob syntax: that child directory.
    Named(String),
    /// A segment whose only glob syntax is `*` and `?`: every child directory
    /// whose name it matches, compared without regard to ASCII case.
    Matching(String),
    /// A segment with a class, brace or group: every child directory.
    Any,
    /// A `**` segment: every directory at any depth below.
    AnyDepth,
}

/// Whether a directory entry named `name` is one [`GlobStep::Matching`]
/// reaches.
#[must_use]
pub fn glob_segment_matches(segment: &str, name: &str) -> bool {
    glob_match(segment, name, None, true)
}

/// How to follow a lookup location that may hold glob syntax. A segment with
/// syntax the matcher does not evaluate is read as matching any name, which
/// only ever adds directories.
#[must_use]
pub fn glob_walk(location: &str) -> GlobWalk {
    // A parameter expansion's braces are not a brace expansion; the shell
    // split also reads the word with each default in its place.
    let syntax = |segment: &str| segment.contains(['*', '?', '[', '{', '(']);
    if location.contains("${") || !syntax(location) {
        return GlobWalk::Literal;
    }
    let flavor = flavor_of(location);
    let Ok(normalized) = normalize_root(location) else {
        return GlobWalk::Literal;
    };
    // The root itself (`/`, `C:/`, `//server/share`) is never a glob.
    let (root, tail) = if let Some(rest) = normalized.strip_prefix("//") {
        let mut parts = rest.splitn(3, '/');
        let server = parts.next().unwrap_or_default();
        let share = parts.next().unwrap_or_default();
        (
            format!("//{server}/{share}"),
            parts.next().unwrap_or_default().to_owned(),
        )
    } else if valid_drive_prefix(&normalized) {
        (normalized[..3].to_owned(), normalized[3..].to_owned())
    } else {
        ("/".to_owned(), normalized[1..].to_owned())
    };
    let segments: Vec<&str> = tail.split('/').filter(|part| !part.is_empty()).collect();
    let Some(first) = segments.iter().position(|segment| syntax(segment)) else {
        return GlobWalk::Literal;
    };
    let mut steps = Vec::new();
    let last = segments.len() - 1;
    for (index, segment) in segments.iter().enumerate().skip(first) {
        if !syntax(segment) {
            steps.push(GlobStep::Named((*segment).to_owned()));
            continue;
        }
        let balanced = |open: char, close: char| {
            segment.matches(open).count() == segment.matches(close).count()
        };
        // A brace or group alternative of `..` or `~…` leaves the root, and
        // so does one a brace expansion builds out of other pieces, or a
        // directory alternative it builds that an older bash expands to `..`
        // (`.{*,x}/`). An expansion too large to list leaves only when its
        // pieces could build such a word.
        let leaves_at = |text: &str| {
            text.split(['{', '}', ',', '(', ')', '|', '@', '!', '+'])
                .any(|piece| piece == ".." || piece.starts_with('~'))
        };
        let leaves = leaves_at(segment)
            || (segment.contains('{')
                && match brace_alternatives(segment) {
                    Some(alternatives) => alternatives.iter().any(|alternative| {
                        alternative.contains(['/', '\\'])
                            || leaves_at(alternative)
                            || (index < last && could_name_parent(alternative))
                    }),
                    None => brace_overflow_could_leave(segment),
                });
        if leaves || !balanced('{', '}') || !balanced('(', ')') {
            return GlobWalk::Unprovable;
        }
        if segment.contains("**") {
            steps.push(GlobStep::AnyDepth);
            break;
        }
        steps.push(if segment.contains(['[', '{', '(']) {
            GlobStep::Any
        } else {
            GlobStep::Matching((*segment).to_owned())
        });
    }
    let mut root_path = root;
    for segment in &segments[..first] {
        if !root_path.ends_with('/') {
            root_path.push('/');
        }
        root_path.push_str(segment);
    }
    GlobWalk::Walk {
        root: native_path(Normalized {
            path: root_path,
            flavor,
        }),
        steps,
    }
}

/// Upper bound on the words [`brace_alternatives`] expands one segment to,
/// and on the braces it reads.
const MAX_BRACE_ALTERNATIVES: usize = 64;

/// Every word a shell's brace expansion makes of `text`: a `{…}` holding a
/// top-level comma, or a sequence (`{1..3}`, `{a..e}`, `{x..y..n}` over
/// integers or single characters), gives one word per alternative, nested
/// and repeated braces included; any other `{…}` stays as written. `None`
/// past [`MAX_BRACE_ALTERNATIVES`].
fn brace_alternatives(text: &str) -> Option<Vec<String>> {
    if text.matches('{').count() > MAX_BRACE_ALTERNATIVES {
        return None;
    }
    let mut words = Vec::new();
    expand_braces(&text.chars().collect::<Vec<_>>(), &mut words)?;
    Some(words)
}

fn expand_braces(text: &[char], words: &mut Vec<String>) -> Option<()> {
    for open in (0..text.len()).filter(|&index| text[index] == '{') {
        let mut depth = 0_usize;
        let mut close = None;
        let mut commas = Vec::new();
        for (index, &c) in text.iter().enumerate().skip(open) {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(index);
                        break;
                    }
                }
                ',' if depth == 1 => commas.push(index),
                _ => {}
            }
        }
        let Some(close) = close else {
            continue;
        };
        let pieces: Vec<Vec<char>> = if commas.is_empty() {
            match brace_sequence(&text[open + 1..close])? {
                Some(pieces) => pieces,
                None => continue,
            }
        } else {
            let mut start = open + 1;
            let mut pieces = Vec::new();
            for end in commas.into_iter().chain(std::iter::once(close)) {
                pieces.push(text[start..end].to_vec());
                start = end + 1;
            }
            pieces
        };
        for piece in pieces {
            let mut word = text[..open].to_vec();
            word.extend(piece);
            word.extend_from_slice(&text[close + 1..]);
            expand_braces(&word, words)?;
        }
        return Some(());
    }
    if words.len() == MAX_BRACE_ALTERNATIVES {
        return None;
    }
    words.push(text.iter().collect());
    Some(())
}

/// The bounds of a brace sequence body (`1..3`, `a..e`, `x..y..n`): the
/// first and last value, whether they are integers (else code points of
/// single characters), and the step. `None` when the body is not one.
fn brace_sequence_bounds(body: &str) -> Option<(i64, i64, bool, u64)> {
    let parts: Vec<&str> = body.split("..").collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let step = match parts.get(2) {
        Some(step) => step.parse::<i64>().ok()?.unsigned_abs().max(1),
        None => 1,
    };
    match (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
        (Ok(from), Ok(to)) => Some((from, to, true, step)),
        _ => {
            let single = |part: &str| {
                let mut chars = part.chars();
                chars.next().filter(|_| chars.next().is_none())
            };
            let (from, to) = (single(parts[0])?, single(parts[1])?);
            Some((
                i64::from(u32::from(from)),
                i64::from(u32::from(to)),
                false,
                step,
            ))
        }
    }
}

/// The words of a brace sequence body: `Some(None)` when the body is not
/// one; `None` past [`MAX_BRACE_ALTERNATIVES`] words.
fn brace_sequence(body: &[char]) -> Option<Option<Vec<Vec<char>>>> {
    let body: String = body.iter().collect();
    let Some((from, to, numeric, step)) = brace_sequence_bounds(&body) else {
        return Some(None);
    };
    let count = from.abs_diff(to) / step + 1;
    if count > MAX_BRACE_ALTERNATIVES as u64 {
        return None;
    }
    let mut words = Vec::new();
    let mut value = from;
    for _ in 0..count {
        let word = if numeric {
            value.to_string().chars().collect()
        } else {
            u32::try_from(value)
                .ok()
                .and_then(char::from_u32)
                .map(|c| vec![c])
                .unwrap_or_default()
        };
        words.push(word);
        let delta = i64::try_from(step).unwrap_or(1);
        value = if from <= to {
            value + delta
        } else {
            value - delta
        };
    }
    Some(Some(words))
}

/// Whether a segment whose brace expansion has more words than
/// [`brace_alternatives`] lists could still build one that leaves the root:
/// one holding a separator, `~` or a group, or one that is `..` or a dot-glob
/// an older bash expands to it. Integer and letter sequences
/// (`file{1..200}.txt`, `{a..h}{1..9}`) build none of these, and neither does
/// a segment with a `.` only where every word also holds another literal
/// character (`app.{1..99}.log`). A character sequence over a range holding
/// `.`, a separator, `~` or a group character leaves.
fn brace_overflow_could_leave(segment: &str) -> bool {
    const LEAVING: [char; 7] = ['.', '/', '\\', '~', '(', ')', '|'];
    // Characters a glob can match a `.` with: never a word's anchor.
    const GLOB: [char; 5] = ['.', '*', '?', '[', ']'];
    if segment.contains(['/', '\\', '~', '(', ')', '|']) {
        return true;
    }
    let chars: Vec<char> = segment.chars().collect();
    // Pair each `{` with its `}` and note the ones holding a comma of their
    // own, in one pass.
    let mut close = vec![None; chars.len()];
    let mut comma = vec![false; chars.len()];
    let mut open = Vec::new();
    for (index, &c) in chars.iter().enumerate() {
        match c {
            '{' => open.push(index),
            '}' => {
                if let Some(start) = open.pop() {
                    close[start] = Some(index);
                }
            }
            ',' => {
                if let Some(&start) = open.last() {
                    comma[start] = true;
                }
            }
            _ => {}
        }
    }
    // `dot`: a word can hold a `.`; `anchor`: every word holds a literal
    // character no glob reads as `.`, so no word is `..` or matches it.
    let (mut dot, mut anchor) = (false, false);
    // The closing indexes of the comma braces the scan is inside.
    let mut ends: Vec<usize> = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if ends.last() == Some(&index) {
            ends.pop();
            index += 1;
            continue;
        }
        if c == '{'
            && let Some(end) = close[index]
        {
            if comma[index] {
                ends.push(end);
                index += 1;
                continue;
            }
            let body: String = chars[index + 1..end].iter().collect();
            if let Some((from, to, numeric, _)) = brace_sequence_bounds(&body) {
                let range = from.min(to)..=from.max(to);
                let holds = |set: &[char]| {
                    set.iter()
                        .any(|&c| range.contains(&i64::from(u32::from(c))))
                };
                if !numeric && holds(&LEAVING) {
                    return true;
                }
                anchor |= ends.is_empty() && (numeric || !holds(&GLOB));
                index = end + 1;
                continue;
            }
        }
        if c == ',' && !ends.is_empty() {
            index += 1;
            continue;
        }
        dot |= c == '.';
        anchor |= ends.is_empty() && !GLOB.contains(&c);
        index += 1;
    }
    dot && !anchor
}

/// The directory a shell tool's relative arguments resolve from: its own
/// `workdir` (absolute, or relative to the `cwd`), else the `cwd`.
fn command_base(workdir: Option<&str>, cwd: &str) -> String {
    // A Windows shell's own spellings of its directory (`/c/x`, `\\?\C:\x`)
    // must not turn the base, and every argument resolved from it, POSIX.
    if let Some(dir) = workdir
        && let Ok(root) = normalize_root(cwd)
        && flavor_of(&root) == Flavor::Windows
        && let dir = strip_powershell_provider(dir)
        && !is_drive_relative(dir)
        && let Some(base) = windows_spelling(dir, &root)
    {
        return base;
    }
    match workdir {
        Some(dir) if is_absolute(dir) => dir.to_owned(),
        // `join` would turn an unusable cwd into `/dir`.
        Some(dir) if is_absolute(cwd) => join(cwd, dir),
        _ => cwd.to_owned(),
    }
}

/// The single-path fields of the file and search/list tool schemas.
const DIRECT_PATH_KEYS: [&str; 7] = [
    "file_path",
    "filePath",
    "path",
    "absolute_path",
    "AbsolutePath",
    "notebook_path",
    "TargetFile",
];

fn direct_paths(object: &Map<String, Value>) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    for key in DIRECT_PATH_KEYS {
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

/// Meta Muse Code's memory tools (Muse Code 1.4.7).
const MUSE_MEMORY_TOOL_NAMES: [&str; 3] = ["read_memory", "add_memory", "edit_memory"];

/// The file a Muse memory tool touches. Its `path` is relative to the memory
/// directory of its `scope`, not to the working directory: the `project`
/// scope is `<repo>/.agents/memory`, committed with the repository, so the
/// path is placed there. The `personal` and `personal_project` scopes (the
/// default) live in Muse's own data directory outside the repository, which
/// no candidate here can name, so they yield `None` and fail closed to
/// metadata-only under an active marker.
fn muse_memory_paths(args: &Value) -> Option<Vec<String>> {
    let object = args.as_object()?;
    if object.get("scope").and_then(Value::as_str) != Some("project") {
        return None;
    }
    let path = object.get("path")?.as_str()?;
    if path.trim().is_empty() || path.chars().count() > MAX_CANDIDATE_PATH_CHARS {
        return None;
    }
    if is_absolute(path) {
        return Some(vec![path.to_owned()]);
    }
    Some(vec![format!(".agents/memory/{path}")])
}

/// Directory arguments of Factory Droid's search/list tools: `LS` lists
/// `directory_path` and `Glob` searches below `folder` (#1240).
const DROID_SEARCH_ROOT_KEYS: [&str; 2] = ["directory_path", "folder"];

/// A search/list tool's explicit directory or file arguments: the shared
/// single-path fields, plus [`DROID_SEARCH_ROOT_KEYS`] for Droid.
fn search_direct_paths(agent: AgentKind, object: &Map<String, Value>) -> Option<Vec<String>> {
    let mut paths = match direct_paths(object) {
        Some(paths) => paths,
        // A shared field that is present but unreadable stays unreadable.
        None if object
            .keys()
            .any(|key| DIRECT_PATH_KEYS.contains(&key.as_str()) || key == "paths") =>
        {
            return None;
        }
        None => Vec::new(),
    };
    if agent == AgentKind::Droid {
        for key in DROID_SEARCH_ROOT_KEYS {
            if let Some(value) = object.get(key) {
                paths.push(value.as_str()?.to_owned());
            }
        }
    }
    (!paths.is_empty()).then_some(paths)
}

/// A shell tool's command as given, split into words on demand: `extract` runs
/// for every tool event and most never need them.
enum ShellCommand<'a> {
    Line(&'a str),
    Argv(Vec<&'a str>),
}

/// A shell tool's words, each with whether a `~` in it was outside quotes
/// (only such a tilde names a directory), plus whether any of them holds
/// syntax the lexical matcher does not evaluate, which makes a miss
/// unprovable.
#[derive(Default)]
struct ShellWords {
    words: Vec<(String, bool)>,
    opaque: bool,
    /// The words of each script that holds such syntax, read as the shell
    /// expands them where that can change which directory a path names: a
    /// zsh glob group kept inside its word and an ANSI-C string with its
    /// numeric escapes decoded. Only the
    /// marker lookup reads them; the matcher already treats such a script
    /// as opaque.
    expanded: Vec<(String, bool)>,
}

/// Longest shell command, in characters summed over its argv elements, the
/// matcher splits. Splitting, matching and the marker-lookup scan are linear
/// in the command (tens of milliseconds per MiB in a release build), but the
/// hook reads an unbounded payload and only the glob walk is timed, so a
/// longer command is unprovable: metadata-only under an active policy, and
/// no marker lookup (the invalid-policy decision, metadata-only for a shell).
pub const MAX_SHELL_COMMAND_CHARS: usize = 1024 * 1024;

impl ShellCommand<'_> {
    /// Longer than [`MAX_SHELL_COMMAND_CHARS`].
    fn oversized(&self) -> bool {
        let over = |text: &str| {
            // Bytes bound characters from above, so most commands are cleared
            // without counting.
            text.len() > MAX_SHELL_COMMAND_CHARS && text.chars().count() > MAX_SHELL_COMMAND_CHARS
        };
        match self {
            Self::Line(command) => over(command),
            Self::Argv(items) => {
                let mut total = 0_usize;
                items.iter().any(|item| {
                    total = total.saturating_add(item.chars().count());
                    total > MAX_SHELL_COMMAND_CHARS
                })
            }
        }
    }

    #[cfg(test)]
    fn words(&self) -> Vec<String> {
        self.split(false)
            .words
            .into_iter()
            .map(|(word, _)| word)
            .collect()
    }

    /// Every word, and again every word inside a word that holds more than
    /// one (`bash -c 'cat x'`, `"$(cat x)"`), down to [`MAX_SHELL_NESTING`]
    /// levels. A script nested deeper is opaque.
    ///
    /// An argument vector (Codex exec) keeps each element as one word
    /// (`private notes/x.md`) and also tokenizes it, so a `bash -lc "<script>"`
    /// element is read like any script. Joining the elements instead would
    /// re-split paths with spaces and let one stray quote swallow the rest.
    ///
    /// `windows` also reads the command without PowerShell and cmd escapes
    /// ([`without_windows_escapes`]), and each reading again with its
    /// backslashes kept as written ([`literal_shell_words`]): PowerShell and
    /// cmd escape nothing with a backslash, so `\\srv\share\x` names a UNC
    /// path there, not the `\srv\share\x` a POSIX shell reads.
    fn split(&self, windows: bool) -> ShellWords {
        let mut split = ShellWords::default();
        // The readings of one command line or argv element.
        let readings = |text: &str| -> Vec<(String, bool)> {
            let mut texts = vec![text.to_owned()];
            texts.extend(windows.then(|| without_windows_escapes(text)).flatten());
            let mut readings = Vec::with_capacity(texts.len() * 2);
            for text in texts {
                let literal = windows && text.contains('\\');
                readings.push((text.clone(), false));
                if literal {
                    readings.push((text, true));
                }
            }
            readings
        };
        match self {
            Self::Line(command) => {
                split.opaque = has_unevaluated_expansion(command);
                if split.opaque {
                    split.expanded.extend(shell_tokens(command, true));
                }
                for (text, literal) in readings(command) {
                    push_nested_words(
                        &split_words(&text, literal),
                        MAX_SHELL_NESTING,
                        literal,
                        &mut split,
                    );
                }
            }
            Self::Argv(items) => {
                // Each element is scanned as a script too, the same
                // over-approximation its words make.
                split.opaque = items.iter().any(|item| has_unevaluated_expansion(item))
                    || runs_inline_code(items);
                for item in items.iter().filter(|item| has_unevaluated_expansion(item)) {
                    split.expanded.extend(shell_tokens(item, true));
                }
                for (index, item) in items.iter().enumerate() {
                    // An element reaches the program as is, so only a script
                    // a shell runs expands a tilde in it.
                    let script = is_shell_script(items, index);
                    for (text, literal) in readings(item) {
                        push_argv_item(&text, script, literal, &mut split);
                    }
                }
            }
        }
        // PowerShell resolves `~` in a path itself, quoted or not.
        if windows {
            for (_, tilde) in &mut split.words {
                *tilde = true;
            }
        }
        // `~-`, `~N` and `~name` name a directory only the running shell
        // knows, so a path under one proves nothing either way.
        split.opaque |= split.words.iter().any(|(word, tilde)| {
            *tilde
                && shell_arguments(word)
                    .iter()
                    .any(|(argument, _)| names_unknown_directory(argument))
        });
        split
    }
}

fn push_argv_item(item: &str, script: bool, literal: bool, split: &mut ShellWords) {
    let tokens = split_words(item, literal);
    if tokens.len() == 1 && tokens[0].0 == item {
        push_word(item, false, split);
        return;
    }
    // Kept whole, a long script would exhaust the match budget.
    if item.chars().count() <= MAX_ARGV_PATH_CHARS {
        split.words.push((item.to_owned(), false));
    }
    push_nested_words(
        &literal_tildes(tokens, script),
        MAX_SHELL_NESTING - 1,
        literal,
        split,
    );
}

/// `tokens` re-split from one word, whose unquoted tildes count only when a
/// shell runs that word as a script; in a quoted message or pattern
/// (`-m "fix ~approx"`, `grep '~[a-z]+'`) a tilde is literal.
fn literal_tildes(tokens: Vec<(String, bool)>, script: bool) -> Vec<(String, bool)> {
    if script {
        return tokens;
    }
    tokens
        .into_iter()
        .map(|(token, _)| (token, false))
        .collect()
}

/// `command` without PowerShell's backquote and cmd's caret escapes, each of
/// which stands for the character after it (`secr`et.md`, `secr^et.md`).
fn without_windows_escapes(command: &str) -> Option<String> {
    if !command.contains(['`', '^']) {
        return None;
    }
    let mut plain = String::with_capacity(command.len());
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        if matches!(c, '`' | '^') {
            plain.extend(chars.next());
        } else {
            plain.push(c);
        }
    }
    Some(plain)
}

/// Whether an argument starts with a tilde prefix whose directory only the
/// running shell knows: `~-` (the previous directory), `~N`/`~+N` (the
/// directory stack) or `~name` (another account's home). `~`, `~/` and `~+`
/// (the working directory) are resolved instead.
fn names_unknown_directory(argument: &str) -> bool {
    let Some(rest) = argument.strip_prefix('~') else {
        return false;
    };
    let prefix = rest.split(['/', '\\']).next().unwrap_or(rest);
    !prefix.is_empty() && prefix != "+"
}

/// `argument` with a leading unquoted `~/` (PowerShell's `~\`) read as the
/// home directory and `~+` as the working directory.
fn expand_tilde(argument: String, tilde: bool, home: Option<&str>, cwd: &str) -> String {
    if !tilde {
        return argument;
    }
    let under_home = argument
        .strip_prefix("~/")
        .or_else(|| argument.strip_prefix("~\\"));
    if let (Some(rest), Some(home)) = (under_home, home) {
        join(home, rest)
    } else if argument == "~+" {
        cwd.to_owned()
    } else if let Some(rest) = argument.strip_prefix("~+/") {
        join(cwd, rest)
    } else {
        argument
    }
}

/// How many quoting levels a script inside a word is re-split.
const MAX_SHELL_NESTING: usize = 4;

/// `literal` re-splits each word with its backslashes kept
/// ([`split_words`]).
fn push_nested_words(
    words: &[(String, bool)],
    depth: usize,
    literal: bool,
    split: &mut ShellWords,
) {
    let texts: Vec<&str> = words.iter().map(|(word, _)| word.as_str()).collect();
    if runs_inline_code(&texts) {
        split.opaque = true;
    }
    for (index, (word, tilde)) in words.iter().enumerate() {
        push_word(word, *tilde, split);
        let tokens = split_words(word, literal);
        if tokens.len() == 1 && tokens[0].0 == *word {
            continue;
        }
        let Some(depth) = depth.checked_sub(1) else {
            split.opaque = true;
            continue;
        };
        // Only a script a shell runs expands its braces, classes and
        // tildes; a quoted message or pattern (`git log --format='%h {a,b}'`)
        // does not.
        let script = word.contains("$(") || word.contains('`') || is_shell_script(&texts, index);
        if script && has_unevaluated_expansion(word) {
            split.opaque = true;
            split.expanded.extend(shell_tokens(word, true));
        }
        push_nested_words(&literal_tildes(tokens, script), depth, literal, split);
    }
}

/// Adds a word, and its POSIX reading when it differs: outside quotes a
/// shell drops the backslash of `\o`, but [`shell_words`] keeps it so a
/// Windows path keeps its separators. A word with a parameter default
/// (`${X:-docs/x.md}`) also adds the reading with the operand in its place,
/// of the word as written and of its POSIX reading (`${X:-d\ocs/x.md}`).
/// `tilde` says whether a `~` in the word was outside quotes.
fn push_word(word: &str, tilde: bool, split: &mut ShellWords) {
    push_word_reading(word, tilde, split);
    if word.contains('\\') {
        let plain = without_backslash_escapes(word);
        if plain != word {
            push_word_reading(&plain, tilde, split);
        }
    }
}

/// `word` as a POSIX shell reads its unquoted backslashes: each escapes the
/// next character.
fn without_backslash_escapes(word: &str) -> String {
    let mut plain = String::with_capacity(word.len());
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            plain.extend(chars.next());
        } else {
            plain.push(c);
        }
    }
    plain
}

fn push_word_reading(word: &str, tilde: bool, split: &mut ShellWords) {
    split.words.push((word.to_owned(), tilde));
    if let Some((operands, exhausted)) = parameter_operands(word) {
        split.words.push((operands, tilde));
        // A default nested deeper than the matcher reads is unprovable.
        split.opaque |= exhausted;
    }
}

/// `word` with each `${NAME:-word}`, `${NAME:=word}` or `${NAME:+word}`
/// (or the form without `:`) replaced by its operand, which the shell
/// substitutes when `NAME` is unset (set, for `+`). Nested operands are
/// replaced down to [`MAX_SHELL_NESTING`] levels; the flag is set when one
/// is nested deeper still.
fn parameter_operands(word: &str) -> Option<(String, bool)> {
    let mut current = replace_parameter_operands(word)?;
    for _ in 1..MAX_SHELL_NESTING {
        match replace_parameter_operands(&current) {
            Some(next) => current = next,
            None => return Some((current, false)),
        }
    }
    let exhausted = replace_parameter_operands(&current).is_some();
    Some((current, exhausted))
}

fn replace_parameter_operands(word: &str) -> Option<String> {
    let mut output = String::new();
    let mut rest = word;
    let mut changed = false;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let body = &rest[start + 2..];
        let name = body
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(body.len());
        let after = &body[name..];
        let operator =
            if after.starts_with(":-") || after.starts_with(":=") || after.starts_with(":+") {
                2
            } else if after.starts_with(['-', '=', '+']) {
                1
            } else {
                0
            };
        let operand = &after[operator..];
        let mut depth = 0_usize;
        let end = operand.char_indices().find_map(|(index, c)| match c {
            '{' => {
                depth += 1;
                None
            }
            '}' if depth == 0 => Some(index),
            '}' => {
                depth -= 1;
                None
            }
            _ => None,
        });
        match end {
            Some(end) if name > 0 && operator > 0 => {
                output.push_str(&operand[..end]);
                changed = true;
                rest = &operand[end + 1..];
            }
            _ => {
                output.push_str("${");
                rest = body;
            }
        }
    }
    output.push_str(rest);
    changed.then_some(output)
}

/// A command word's program name: its last path segment, lowercased,
/// without `.exe`.
fn command_name(word: &str) -> String {
    let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
    let name = name.to_ascii_lowercase();
    if let Some(stem) = name.strip_suffix(".exe") {
        return stem.to_owned();
    }
    name
}

/// Whether `words[index]` is a script a shell runs: the argument after
/// `sh -c` (any flag cluster with `c`, as in `bash -lc`, past other options
/// and their values, as in `bash -o pipefail -c`), after `su -c` or
/// `script -c`, which hand it to a shell, or after `eval`.
fn is_shell_script<S: AsRef<str>>(words: &[S], index: usize) -> bool {
    let mut script_flag = false;
    for previous in (0..index).rev() {
        let word = words[previous].as_ref();
        if let Some(flag) = word.strip_prefix('-') {
            script_flag |= !flag.starts_with('-') && flag.contains('c');
            continue;
        }
        // `+o pipefail`, `+x`.
        if word.len() > 1 && word.starts_with('+') {
            continue;
        }
        if previous > 0 && takes_shell_value(words[previous - 1].as_ref()) {
            continue;
        }
        let name = command_name(word);
        return (script_flag
            && matches!(
                name.as_str(),
                "sh" | "bash"
                    | "zsh"
                    | "dash"
                    | "ksh"
                    | "mksh"
                    | "ash"
                    | "yash"
                    | "fish"
                    | "su"
                    | "script"
            ))
            || (previous + 1 == index && name == "eval");
    }
    false
}

/// Whether a shell option consumes the next word as its value: `-o`/`+o`
/// and `-O`/`+O` (also ending a flag cluster, as in `-eo`), `--rcfile`,
/// `--init-file`, and fish's `-C`/`--init-command`.
fn takes_shell_value(flag: &str) -> bool {
    matches!(flag, "--rcfile" | "--init-file" | "--init-command" | "-C")
        || (flag.len() > 1
            && (flag.starts_with('+') || (flag.starts_with('-') && !flag.starts_with("--")))
            && flag.ends_with(['o', 'O']))
}

/// Whether the words run inline interpreter code (`python3 -c`, `node -e`,
/// `ruby -e`, `perl -e`, `php -r`, `lua -e`, `Rscript -e`, `osascript -e`,
/// `deno eval`). Code builds paths at run
/// time (`'docs/' + name`), so no lexical match can rule an ignored path out.
fn runs_inline_code<S: AsRef<str>>(words: &[S]) -> bool {
    (0..words.len()).any(|index| {
        let name = command_name(words[index].as_ref());
        let versioned = |prefix: &str| {
            name.strip_prefix(prefix)
                .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit() || c == '.'))
        };
        // The inline-code flag letters, and the options that take the
        // next word as their value (`python3 -W ignore -c …`).
        let (letters, values): (&[char], &[&str]) =
            if versioned("python") || versioned("pypy") || name == "py" {
                (&['c'], &["-W", "-X", "--check-hash-based-pycs"])
            } else if versioned("perl") {
                (&['e', 'E'], &["-I", "-M", "-m"])
            } else if versioned("ruby") {
                (&['e'], &["-I", "-r", "-C", "-E"])
            } else if matches!(name.as_str(), "node" | "nodejs" | "bun") {
                (
                    &['e', 'p'],
                    &[
                        "-r",
                        "--require",
                        "--import",
                        "--loader",
                        "--experimental-loader",
                        "-C",
                        "--conditions",
                        "--input-type",
                        "--title",
                        "--env-file",
                    ],
                )
            } else if versioned("php") {
                (&['r'], &["-d", "-c", "-z"])
            } else if matches!(name.as_str(), "lua" | "luajit" | "rscript" | "osascript") {
                (&['e'], &["-l"])
            } else if name == "deno" {
                return words
                    .get(index + 1)
                    .is_some_and(|next| next.as_ref() == "eval");
            } else {
                return false;
            };
        let mut rest = words[index + 1..].iter().map(AsRef::as_ref);
        while let Some(flag) = rest.next() {
            if !flag.starts_with('-') {
                return false;
            }
            if matches!(flag, "--eval" | "--print")
                || flag.starts_with("--eval=")
                || flag.starts_with("--print=")
                || (!flag.starts_with("--") && flag[1..].contains(letters))
            {
                return true;
            }
            if values.contains(&flag) {
                rest.next();
            }
        }
        false
    })
}

/// Whether an unquoted word of `command` holds syntax the lexical matcher
/// does not evaluate: a brace expansion (`{a,b}`, `{1..3}`), a bracket class
/// (`[ab]`, `[]a]`), a zsh glob group or extglob (`docs/(a|b)`, `(a|b)/x`,
/// `@(x)`), or an ANSI-C string with a numeric or control escape
/// (`$'\x64'`). Quoted or escaped braces and brackets are literal, `${…}` is
/// a parameter expansion, `$(…)`, `<(…)` and a standalone `( … )` subshell
/// are not globs, and `{}` or a lone `[` (`find -exec … {}`, `[ -f x ]`)
/// expands nothing.
fn has_unevaluated_expansion(command: &str) -> bool {
    let is_operator = |c: char| matches!(c, '|' | '&' | ';' | '<' | '>' | '(' | ')' | '`');
    let is_word = |c: Option<&char>| c.is_some_and(|c| !c.is_whitespace() && !is_operator(*c));
    let mut quote = None;
    let mut previous = None;
    let mut in_word = false;
    let mut parameter = false;
    let mut brace = None::<bool>;
    let mut class = None::<usize>;
    // One entry per open unquoted `(`: whether it starts a word, where zsh
    // reads a following glob (`(a|b)/x`) instead of a subshell.
    let mut groups = Vec::new();
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(open) = quote {
            match (open, c) {
                ('$' | '\'', '\'') | ('"', '"') => quote = None,
                // The guard consumes the escaped character either way.
                ('$', '\\')
                    if chars.next().is_some_and(|escape| {
                        matches!(escape, 'x' | 'u' | 'U' | 'c' | '0'..='7')
                    }) =>
                {
                    return true;
                }
                ('"', '\\') => {
                    chars.next();
                }
                _ => {}
            }
            class = class.map(|len| len + 1);
            previous = None;
            in_word = true;
            continue;
        }
        match c {
            // `$'…'` is an ANSI-C string, tracked as its own quote.
            '\'' if previous == Some('$') => quote = Some('$'),
            '\'' | '"' => quote = Some(c),
            '\\' => {
                chars.next();
                class = class.map(|len| len + 1);
                previous = None;
                in_word = true;
                continue;
            }
            '(' => {
                if in_word && !matches!(previous, Some('$' | '=')) {
                    return true;
                }
                groups.push(!in_word && !matches!(previous, Some('<' | '>')));
                (parameter, brace, class, in_word) = (false, None, None, false);
            }
            ')' => {
                if groups.pop() == Some(true) && is_word(chars.peek()) {
                    return true;
                }
                (parameter, brace, class, in_word) = (false, None, None, false);
            }
            '|' if groups.last() == Some(&true) && in_word && is_word(chars.peek()) => {
                return true;
            }
            c if c.is_whitespace() || is_operator(c) => {
                (parameter, brace, class, in_word) = (false, None, None, false);
            }
            '{' if previous == Some('$') => parameter = true,
            '}' if parameter => parameter = false,
            _ if parameter => {}
            '{' => brace = Some(false),
            ',' if brace.is_some() => brace = Some(true),
            '.' if brace.is_some() && previous == Some('.') => brace = Some(true),
            '}' if brace.is_some() => {
                if brace == Some(true) {
                    return true;
                }
                brace = None;
            }
            '[' if class.is_none() => class = Some(0),
            // A `]` first in a class is a member (`[]a]`), not its end.
            ']' if class == Some(0) => class = Some(1),
            ']' if class.is_some() => return true,
            _ => class = class.map(|len| len + 1),
        }
        if !c.is_whitespace() && !is_operator(c) {
            in_word = true;
        }
        previous = Some(c);
    }
    false
}

fn shell_command(args: &Value) -> Option<ShellCommand<'_>> {
    match args
        .get("command")
        .or_else(|| args.get("cmd"))
        .or_else(|| args.get("CommandLine"))?
    {
        Value::String(command) => Some(ShellCommand::Line(command)),
        Value::Array(items) => items
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .map(ShellCommand::Argv),
        _ => None,
    }
}

/// Splits a command line into words the way a POSIX shell would before
/// expansion: quotes group, and whitespace and control/redirection operators
/// separate. A backslash escapes only a character the shell treats specially,
/// so Windows paths (`docs\adr\x.md`) keep their separators. A backquote
/// opens or closes a command substitution, so it separates too. An ANSI-C
/// string (`$'…'`) has its escapes decoded; numeric and control escapes stay
/// as written, and [`has_unevaluated_expansion`] reports them.
#[cfg(test)]
fn shell_words(command: &str) -> Vec<String> {
    marked_shell_words(command)
        .into_iter()
        .map(|(word, _)| word)
        .collect()
}

/// [`shell_words`], each word with whether it holds a `~` outside quotes and
/// not after a backslash, the only tilde a shell expands.
fn marked_shell_words(command: &str) -> Vec<(String, bool)> {
    shell_tokens(command, false)
}

/// [`marked_shell_words`]; with `expand`, a zsh glob group stays inside its
/// word (`docs/(a|b)/x`, `(a|b)/x`) instead of splitting it as a subshell, and
/// the numeric and control escapes of an ANSI-C string are decoded.
fn shell_tokens(command: &str, expand: bool) -> Vec<(String, bool)> {
    tokenize(command, expand, false)
}

/// [`marked_shell_words`] with every backslash an ordinary character, as
/// PowerShell and cmd read it: `\\srv\share\x`, bare or double-quoted, keeps
/// both leading backslashes.
fn literal_shell_words(command: &str) -> Vec<(String, bool)> {
    tokenize(command, false, true)
}

/// The words of `command` read with POSIX quoting ([`shell_tokens`]), or,
/// with `literal`, with backslashes kept as written ([`literal_shell_words`]).
fn split_words(command: &str, literal: bool) -> Vec<(String, bool)> {
    if literal {
        literal_shell_words(command)
    } else {
        marked_shell_words(command)
    }
}

/// [`shell_tokens`]; `literal` reads every backslash as an ordinary character.
fn tokenize(command: &str, expand: bool, literal: bool) -> Vec<(String, bool)> {
    let is_operator = |c: char| matches!(c, '|' | '&' | ';' | '<' | '>' | '(' | ')' | '`');
    let mut words = Vec::new();
    let mut word = String::new();
    let mut tilde = false;
    let mut in_word = false;
    let mut quote = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('\'' | '$'), '\'') | (Some('"'), '"') => quote = None,
            // A backslash-newline outside single quotes continues the line.
            (Some('"') | None, '\\') if !literal && chars.peek() == Some(&'\n') => {
                chars.next();
            }
            // Inside double quotes a backslash escapes `"`, `\`, `$` and a
            // backquote, so a script a shell re-reads (`bash -c "cat \$'…'"`)
            // sees the `$` itself.
            (Some('"'), '\\')
                if !literal && matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) =>
            {
                word.extend(chars.next());
            }
            (Some('$'), '\\') if !literal => match chars.next() {
                Some(escape) => match ansi_c_escape(escape).or_else(|| {
                    expand
                        .then(|| ansi_c_numeric_escape(escape, &mut chars))
                        .flatten()
                }) {
                    Some(decoded) => word.push(decoded),
                    None => {
                        word.push('\\');
                        word.push(escape);
                    }
                },
                None => word.push('\\'),
            },
            (Some(_), _) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            // `$'…'` (ANSI-C) and `$"…"` (locale) strings drop the `$`.
            (None, '$') if matches!(chars.peek(), Some('\'' | '"')) => {
                quote = match chars.next() {
                    Some('\'') => Some('$'),
                    _ => Some('"'),
                };
                in_word = true;
            }
            (None, '\\')
                if !literal
                    && chars.peek().is_some_and(|next| {
                        next.is_whitespace()
                            || is_operator(*next)
                            || matches!(next, '\'' | '"' | '\\')
                    }) =>
            {
                word.extend(chars.next());
                in_word = true;
            }
            (None, '(')
                if expand
                    && (!in_word || !word.ends_with(['$', '=']))
                    && let Some((group, rest)) = glob_group(&chars, in_word) =>
            {
                word.push_str(&group);
                chars = rest;
                in_word = true;
            }
            (None, c) if c.is_whitespace() || is_operator(c) => {
                if in_word {
                    words.push((std::mem::take(&mut word), tilde));
                    (in_word, tilde) = (false, false);
                }
            }
            (None, c) => {
                tilde |= c == '~' && !word.ends_with('\\');
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push((word, tilde));
    }
    words
}

/// The zsh glob group an unquoted `(` opens, `(` through its matching `)`,
/// and the characters after it: inside a word (`docs/(a|b)/x`), or at the start
/// of one when a word character follows the `)` (`(a|b)/x`). `None` for a
/// subshell, or when whitespace, a quote, an escape or another operator comes
/// first.
fn glob_group<'a>(
    after_open: &std::iter::Peekable<std::str::Chars<'a>>,
    in_word: bool,
) -> Option<(String, std::iter::Peekable<std::str::Chars<'a>>)> {
    let mut ahead = after_open.clone();
    let mut group = String::from("(");
    let mut depth = 1_usize;
    while depth > 0 {
        let c = ahead.next()?;
        if c.is_whitespace() || matches!(c, '\'' | '"' | '\\' | ';' | '&' | '<' | '>' | '`') {
            return None;
        }
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        group.push(c);
    }
    let word_follows = ahead.peek().is_some_and(|next| {
        !next.is_whitespace() && !matches!(next, '|' | '&' | ';' | '<' | '>' | '(' | ')' | '`')
    });
    (in_word || word_follows).then_some((group, ahead))
}

/// The character a numeric or control ANSI-C escape stands for, reading its
/// digits from `rest`: `\xHH`, `\uHHHH`, `\UHHHHHHHH`, `\NNN` (octal) and
/// `\cX`. `None` when it spells no character.
fn ansi_c_numeric_escape(
    escape: char,
    rest: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Option<char> {
    let (radix, mut value, most) = match escape {
        'x' => (16, 0, 2),
        'u' => (16, 0, 4),
        'U' => (16, 0, 8),
        '0'..='7' => (8, escape.to_digit(8)?, 2),
        'c' => {
            let control = rest.next_if(char::is_ascii)?;
            return Some(char::from(control as u8 & 0x1f));
        }
        _ => return None,
    };
    // Digits are consumed only when they spell a character.
    let mut ahead = rest.clone();
    let mut digits = 0;
    while digits < most {
        let Some(digit) = ahead.peek().and_then(|c| c.to_digit(radix)) else {
            break;
        };
        value = value * radix + digit;
        ahead.next();
        digits += 1;
    }
    let decoded = (radix == 8 || digits > 0)
        .then(|| char::from_u32(value))
        .flatten()?;
    *rest = ahead;
    Some(decoded)
}

/// The character an ANSI-C escape stands for, or `None` for a numeric or
/// control escape (`\x64`, `\144`, `\u0064`, `\cA`) that is not decoded.
fn ansi_c_escape(escape: char) -> Option<char> {
    Some(match escape {
        'a' => '\u{7}',
        'b' => '\u{8}',
        'e' | 'E' => '\u{1b}',
        'f' => '\u{c}',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\u{b}',
        '\\' | '\'' | '"' | '?' => escape,
        _ => return None,
    })
}

/// Path-like values carried by one shell word: the word itself unless it is
/// a flag, plus the value of a `--flag=value`, `NAME=value`, or PowerShell
/// `-Param:value` word, plus a value glued to a short flag (`-fFILE`), plus
/// the path of a git `REV:path` object name, plus the path of any of these
/// that is a `file:` URL. The flag is set for the object name's path, which
/// git resolves from the repository root.
fn shell_arguments(word: &str) -> Vec<(String, bool)> {
    let flag = word.starts_with('-');
    let mut arguments: Vec<(String, bool)> = Vec::new();
    if !flag {
        arguments.push((word.to_owned(), false));
    }
    if let Some((_, value)) = word
        .split_once('=')
        .or_else(|| flag.then(|| word.split_once(':')).flatten())
    {
        arguments.push((value.to_owned(), false));
    }
    if let Some(cluster) = word.strip_prefix('-').filter(|rest| !rest.starts_with('-')) {
        let mut chars = cluster.chars();
        chars.next();
        if !chars.as_str().is_empty() {
            arguments.push((chars.as_str().to_owned(), false));
        }
    }
    if let Some(path) = git_object_path(word) {
        arguments.push((path.to_owned(), true));
    }
    let urls: Vec<_> = arguments
        .iter()
        .filter_map(|(argument, _)| file_url_path(argument))
        .map(|path| (path, false))
        .collect();
    arguments.extend(urls);
    arguments.retain(|(argument, _)| !argument.trim().is_empty());
    arguments
}

/// The local path a `file:` URL names (`file:///x`, `file://host/x`,
/// `file:/x`, any case), with ASCII percent-escapes decoded. A query or
/// fragment (`?…`, `#…`) is not part of the path; an escaped `%3F` or `%23`
/// is, so the URL is cut before decoding.
fn file_url_path(argument: &str) -> Option<String> {
    if !argument.get(..5)?.eq_ignore_ascii_case("file:") {
        return None;
    }
    let rest = &argument[5..];
    let rest = rest.find(['?', '#']).map_or(rest, |end| &rest[..end]);
    let path = match rest.strip_prefix("//") {
        Some(authority) => &authority[authority.find('/')?..],
        None => rest,
    };
    // `file:///C:/x` names the drive path `C:/x`.
    let path = path
        .strip_prefix('/')
        .filter(|drive| valid_drive_prefix(drive))
        .unwrap_or(path);
    let mut decoded = String::with_capacity(path.len());
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        let escape = chars.as_str().get(..2).and_then(|hex| {
            u8::from_str_radix(hex, 16)
                .ok()
                .filter(|byte| byte.is_ascii() && hex.chars().all(|d| d.is_ascii_hexdigit()))
        });
        match escape {
            Some(byte) if c == '%' => {
                decoded.push(char::from(byte));
                chars.nth(1);
            }
            _ => decoded.push(c),
        }
    }
    Some(decoded)
}

/// The path of a git object name such as `HEAD:docs/x.md`, `v1.2~3:x`,
/// `:docs/x.md` (the index) or `:2:x` (a merge stage). A drive letter
/// (`C:\x`), a URL (`https://…`) and anything whose left side is not
/// revision-shaped (`host:path`, `NAME=a:b`) are not object names here.
fn git_object_path(word: &str) -> Option<&str> {
    if word.starts_with('-') {
        return None;
    }
    let (revision, path) = word.split_once(':')?;
    let path = if revision.is_empty() {
        path.strip_prefix(['0', '1', '2', '3'])
            .and_then(|rest| rest.strip_prefix(':'))
            .unwrap_or(path)
    } else {
        path
    };
    let drive = revision.len() == 1 && revision.as_bytes()[0].is_ascii_alphabetic();
    let revision_shaped = revision.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '.' | '_' | '-' | '/' | '~' | '^' | '@' | '{' | '}')
    });
    (!drive && revision_shaped && !path.starts_with("//")).then_some(path)
}

#[derive(Clone)]
struct Normalized {
    path: String,
    flavor: Flavor,
}
/// [`normalize_candidates`]'s first reading.
fn normalize_candidate(candidate: &str, cwd: &str) -> Option<Normalized> {
    normalize_candidates(candidate, cwd)?.into_iter().next()
}

/// Every normalized path a candidate can name ([`candidate_raw`]); `None`
/// when it is not a usable path.
fn normalize_candidates(candidate: &str, cwd: &str) -> Option<Vec<Normalized>> {
    let (raws, flavor) = candidate_raw(candidate, cwd)?;
    raws.iter()
        .map(|raw| {
            Some(Normalized {
                path: normalize_segments(raw)?,
                flavor,
            })
        })
        .collect()
}

/// Upper bound on the readings [`normalize_readings`] gives one glob.
const MAX_GLOBSTAR_READINGS: usize = 16;

/// [`normalize_candidate`] for a glob, whose `**` a shell (zsh, bash with
/// `globstar`) and a glob tool expand to no directory as well as to many. A
/// `..` after it then steps back from the directory before the `**` (or from
/// one it matched), which a lexical `..` that removes the `**` misses: every
/// such `..` gives two readings, the `**` matching nothing (the `..` steps
/// back from the segment before it) and the `**` matching at least one
/// directory (the `**` stays). A directory segment that an older bash can
/// expand to `..` ([`could_name_parent`]) is also read as `..`. `Ok(None)`
/// when the candidate is not a usable path; `Err` past
/// [`MAX_GLOBSTAR_READINGS`], which proves nothing.
fn normalize_readings(candidate: &str, cwd: &str) -> Result<Option<Vec<Normalized>>, ()> {
    let Some((raws, flavor)) = candidate_raw(candidate, cwd) else {
        return Ok(None);
    };
    let mut all = Vec::new();
    for raw in &raws {
        let Some(readings) = raw_readings(raw, flavor)? else {
            return Ok(None);
        };
        all.extend(readings);
        if all.len() > MAX_GLOBSTAR_READINGS {
            return Err(());
        }
    }
    Ok(Some(all))
}

/// [`normalize_readings`] for one joined path.
fn raw_readings(raw: &str, flavor: Flavor) -> Result<Option<Vec<Normalized>>, ()> {
    let Some((root, tail)) = split_root(raw) else {
        return Ok(None);
    };
    let segments: Vec<&str> = tail
        .into_iter()
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    let mut readings: Vec<Vec<&str>> = vec![Vec::new()];
    for (index, &part) in segments.iter().enumerate() {
        // A directory segment an older bash can expand to `..` is read as
        // that too.
        let directory = index + 1 < segments.len();
        let parent = directory && part != ".." && could_name_parent(part);
        let mut next: Vec<Vec<&str>> = Vec::with_capacity(readings.len());
        for parts in readings {
            if parent {
                let mut up = parts.clone();
                up.pop();
                keep(&mut next, up);
            }
            if part != ".." {
                let mut down = parts;
                down.push(part);
                keep(&mut next, down);
            } else if parts.last().is_some_and(|last| last.contains("**")) {
                let mut none = parts.clone();
                none.pop();
                none.pop();
                keep(&mut next, none);
                keep(&mut next, parts);
            } else {
                let mut up = parts;
                up.pop();
                keep(&mut next, up);
            }
        }
        if next.len() > MAX_GLOBSTAR_READINGS {
            return Err(());
        }
        readings = next;
    }
    Ok(Some(
        readings
            .into_iter()
            .map(|parts| Normalized {
                path: join_segments(root.clone(), &parts),
                flavor,
            })
            .collect(),
    ))
}

/// Adds a reading unless an equal one is already there.
fn keep<'a>(readings: &mut Vec<Vec<&'a str>>, parts: Vec<&'a str>) {
    if !readings.contains(&parts) {
        readings.push(parts);
    }
}

/// Whether a glob segment can match `..` in a shell that, like bash before
/// 5.2, lets a pattern with a leading `.` match the `.` and `..` entries
/// (`.*`, `.?`, a class after the dot that admits `.`, `[[:punct:]]` among
/// them, or an extglob group with an alternative that matches `.`, as in
/// `.@(.)` or `.!(x)`). zsh and newer bash never do; reading the segment as
/// `..` as well only adds a reading.
fn could_name_parent(segment: &str) -> bool {
    let Some(rest) = segment.strip_prefix('.') else {
        return false;
    };
    rest.contains(['*', '?', '[', '('])
        && class_glob_matches(&rest.chars().collect::<Vec<_>>(), &['.'])
}

/// A one-segment glob over `*`, `?`, `[…]` classes (`!` or `^` negates,
/// `a-z` ranges, a leading `]` is a member, `[:name:]`, `[=c=]` and `[.c.]`
/// inside) and extglob groups (`@(…)`, `?(…)`, `*(…)`, `+(…)`, `!(…)`, `|`
/// between alternatives) matched against `text`. A `[` without a closing `]`
/// is literal, and so is a `(` without its `)`. Meant for short texts: a
/// group tries every split of the text.
fn class_glob_matches(pattern: &[char], text: &[char]) -> bool {
    if let [kind @ ('@' | '?' | '*' | '+' | '!'), '(', ..] = pattern
        && let Some(close) = group_close(pattern, 1)
    {
        let alternatives = group_alternatives(&pattern[2..close]);
        let rest = &pattern[close + 1..];
        return (0..=text.len()).any(|split| {
            extglob_group_matches(*kind, &alternatives, &text[..split])
                && class_glob_matches(rest, &text[split..])
        });
    }
    match pattern.first() {
        None => text.is_empty(),
        Some('*') => (0..=text.len()).any(|skip| class_glob_matches(&pattern[1..], &text[skip..])),
        Some('?') => !text.is_empty() && class_glob_matches(&pattern[1..], &text[1..]),
        Some('[') => {
            let Some((close, members)) = bracket_class(pattern) else {
                return text.first() == Some(&'[') && class_glob_matches(&pattern[1..], &text[1..]);
            };
            let Some(&c) = text.first() else {
                return false;
            };
            members(c) && class_glob_matches(&pattern[close + 1..], &text[1..])
        }
        Some(&literal) => {
            text.first() == Some(&literal) && class_glob_matches(&pattern[1..], &text[1..])
        }
    }
}

/// The index of the `)` closing the group whose `(` is at `open`.
fn group_close(pattern: &[char], open: usize) -> Option<usize> {
    let mut depth = 0_usize;
    for (index, &c) in pattern.iter().enumerate().skip(open) {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// The `|`-separated alternatives of a group body, nested groups kept whole.
fn group_alternatives(body: &[char]) -> Vec<&[char]> {
    let mut alternatives = Vec::new();
    let (mut depth, mut start) = (0_usize, 0);
    for (index, &c) in body.iter().enumerate() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '|' if depth == 0 => {
                alternatives.push(&body[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    alternatives.push(&body[start..]);
    alternatives
}

/// Whether the extglob group `kind(alternatives)` matches all of `text`.
fn extglob_group_matches(kind: char, alternatives: &[&[char]], text: &[char]) -> bool {
    let once = |text: &[char]| {
        alternatives
            .iter()
            .any(|alternative| class_glob_matches(alternative, text))
    };
    // One or more alternatives in a row (`+`), or none as well (`*`).
    fn repeated(once: &dyn Fn(&[char]) -> bool, text: &[char]) -> bool {
        (1..=text.len()).any(|split| {
            once(&text[..split]) && (split == text.len() || repeated(once, &text[split..]))
        })
    }
    match kind {
        '@' => once(text),
        '?' => text.is_empty() || once(text),
        '*' => text.is_empty() || repeated(&once, text),
        '+' => repeated(&once, text),
        _ => !once(text),
    }
}

/// The `]` closing the bracket class that opens `pattern`, and whether the
/// class admits a character. `None` when no `]` closes it.
fn bracket_class(pattern: &[char]) -> Option<(usize, impl Fn(char) -> bool + '_)> {
    let negated = matches!(pattern.get(1), Some('!' | '^'));
    let start = if negated { 2 } else { 1 };
    // A `]` right after the opening (and negation) is a member.
    let mut index = start + usize::from(pattern.get(start) == Some(&']'));
    let mut members: Vec<ClassMember> = Vec::new();
    if index > start {
        members.push(ClassMember::Char(']'));
    }
    loop {
        match pattern.get(index)? {
            ']' => break,
            '[' if matches!(pattern.get(index + 1), Some(':' | '=' | '.')) => {
                let kind = pattern[index + 1];
                let end = (index + 2..pattern.len().saturating_sub(1))
                    .find(|&at| pattern[at] == kind && pattern[at + 1] == ']');
                if let Some(end) = end {
                    let name: String = pattern[index + 2..end].iter().collect();
                    members.push(if kind == ':' {
                        ClassMember::Named(name)
                    } else {
                        ClassMember::Symbol(name)
                    });
                    index = end + 2;
                } else {
                    members.push(ClassMember::Char('['));
                    index += 1;
                }
            }
            &low if pattern.get(index + 1) == Some(&'-')
                && pattern.get(index + 2).is_some_and(|&high| high != ']') =>
            {
                members.push(ClassMember::Range(low, pattern[index + 2]));
                index += 3;
            }
            &c => {
                members.push(ClassMember::Char(c));
                index += 1;
            }
        }
    }
    let admits = move |c: char| members.iter().any(|member| member.admits(c)) != negated;
    Some((index, admits))
}

/// One member of a bracket class.
enum ClassMember {
    Char(char),
    Range(char, char),
    /// `[:name:]`.
    Named(String),
    /// `[=c=]` or `[.c.]`.
    Symbol(String),
}

impl ClassMember {
    fn admits(&self, c: char) -> bool {
        match self {
            Self::Char(member) => *member == c,
            Self::Range(low, high) => (*low..=*high).contains(&c),
            Self::Named(name) => match name.as_str() {
                "alnum" => c.is_alphanumeric(),
                "alpha" => c.is_alphabetic(),
                "blank" => c == ' ' || c == '\t',
                "cntrl" => c.is_control(),
                "digit" => c.is_ascii_digit(),
                "lower" => c.is_lowercase(),
                "space" => c.is_whitespace(),
                "upper" => c.is_uppercase(),
                "word" => c.is_alphanumeric() || c == '_',
                "xdigit" => c.is_ascii_hexdigit(),
                "punct" => c.is_ascii_punctuation(),
                "graph" => c.is_ascii_graphic(),
                "print" => c.is_ascii_graphic() || c == ' ',
                "ascii" => c.is_ascii(),
                // A class this reading does not know may admit anything.
                _ => true,
            },
            // A single character, or a named collating element that may be it.
            Self::Symbol(symbol) => {
                let mut chars = symbol.chars();
                match (chars.next(), chars.next()) {
                    (Some(only), None) => only == c,
                    _ => true,
                }
            }
        }
    }
}

/// A candidate joined to `cwd` when relative, with the flavor its matching
/// uses, before segment normalization: every path it can name, more than one
/// only for an ambiguous Windows spelling ([`windows_spellings`]).
fn candidate_raw(candidate: &str, cwd: &str) -> Option<(Vec<String>, Flavor)> {
    if candidate.trim().is_empty() || candidate.chars().count() > MAX_CANDIDATE_PATH_CHARS {
        return None;
    }
    let cwd = normalize_root(cwd).ok()?;
    if flavor_of(&cwd) == Flavor::Windows {
        let candidate = strip_powershell_provider(candidate);
        if is_drive_relative(candidate) {
            return None;
        }
        return Some((windows_spellings(candidate, &cwd)?, Flavor::Windows));
    }
    if is_drive_relative(candidate) {
        return None;
    }
    // Flavor must come from the host (the cwd), never from the candidate
    // string alone: on a POSIX host a leading `//` is an ordinary doubled
    // separator, not a UNC root, but `flavor_of` cannot tell the two apart
    // from the string in isolation. Collapsing it first keeps a POSIX
    // candidate POSIX-flavored so it still matches a POSIX `ignore_paths`
    // pattern instead of silently escaping every pattern via a flavor
    // mismatch (GHSA-vh98). A genuine Windows/UNC host is unaffected: the
    // collapse only runs when the cwd itself is not windows-flavored.
    let raw = if is_absolute(candidate) {
        candidate.to_owned()
    } else {
        join(&cwd, candidate)
    };
    let raw = if flavor_of(&cwd) == Flavor::Posix && raw.starts_with("//") {
        format!("/{}", raw.trim_start_matches('/'))
    } else {
        raw
    };
    let flavor = flavor_of(&raw);
    Some((vec![raw], flavor))
}
/// The path a PowerShell provider-qualified spelling names: PowerShell opens
/// `FileSystem::C:\x` and `Microsoft.PowerShell.Core\FileSystem::C:\x` (any
/// case) as `C:\x`. Only a Windows session reads it this way.
fn strip_powershell_provider(path: &str) -> &str {
    const PREFIXES: [&str; 3] = [
        "microsoft.powershell.core\\filesystem::",
        "microsoft.powershell.core/filesystem::",
        "filesystem::",
    ];
    let mut rest = path;
    while let Some(prefix) = PREFIXES.iter().find(|prefix| {
        rest.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    }) {
        rest = &rest[prefix.len()..];
    }
    rest
}

/// Whether `cwd` (a session's cwd or a shell's base directory) is a Windows
/// one.
fn windows_session(cwd: &str) -> bool {
    normalize_root(cwd).is_ok_and(|root| flavor_of(&root) == Flavor::Windows)
}

/// Never-match `ignore_paths` entry used by server re-inspection to compile
/// an Active policy without seeing the client's real patterns. The spelling
/// must share the cwd's path flavor: a POSIX absolute sentinel is rejected
/// on a Windows repository (GHSA-gcpc-88hm-3xqr), which would make every
/// kept file and shell event metadata-only (#1250).
pub(crate) fn inspection_sentinel(cwd: &str) -> &'static str {
    if windows_session(cwd) {
        "C:/__ai_memory_capture_server_inspection_never_match__"
    } else {
        "/__ai_memory_capture_server_inspection_never_match__"
    }
}

/// Whether a Windows shell word names a file the matcher cannot place, so a
/// command holding it proves no miss: a drive-relative path (`C:docs\x`, a
/// bare `C:`), which Windows resolves from that drive's own current directory
/// that only the shell knows, or a device-namespace path naming no drive or
/// share (`\\?\GLOBALROOT\Device\...`, `\\.\HarddiskVolume3\...`,
/// `\\?\Volume{GUID}\...`). A flag (`/s`), a URL and an ordinary path are
/// placeable.
fn windows_unplaceable(word: &str, cwd: &str) -> bool {
    let word = strip_powershell_provider(word);
    let bytes = word.as_bytes();
    if bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && !matches!(bytes.get(2), Some(b'/' | b'\\'))
    {
        return true;
    }
    let path = word.replace('\\', "/");
    ["//?/", "//./", "/??/"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
        && normalize_root(cwd)
            .ok()
            .and_then(|cwd| windows_spellings(word, &cwd))
            .is_none()
}

/// The absolute path Windows opens for `candidate` from the normalized
/// Windows `cwd`, spelled with `/`, its `.` and `..` segments still in place.
/// Every spelling of one file comes out the same, because an ignore pattern
/// names only one of them:
///
/// - the Win32 device and NT prefixes `\\?\`, `\\.\` and `\??\` before a
///   drive or `UNC\server\share`, and the administrative share of the local
///   machine (`\\localhost\C$`, `\\127.0.0.1\C$`), name the drive or share
///   itself; any other device (a volume GUID, a pipe) is no usable path;
/// - Git Bash, MSYS2 and Cygwin spell `C:\x` as `/c/x` and `/cygdrive/c/x`;
/// - any other rooted path (`/x`, `\x`) is on the cwd's drive or share;
/// - Windows drops an alternate data stream suffix (`name:stream`,
///   `name::$DATA`, the file's own content) and the trailing dots and spaces
///   of every segment (`private.md.`, `secrets. `).
///
/// [`windows_spellings`]'s first reading: Git Bash's for `/c/x`.
fn windows_spelling(candidate: &str, cwd: &str) -> Option<String> {
    windows_spellings(candidate, cwd)?.into_iter().next()
}

/// Every path Windows can open for `candidate` ([`windows_spelling`]): Git
/// Bash, MSYS2 and Cygwin read `/c/x` and `/cygdrive/c/x` as `C:\x`, but a
/// native tool reads them from the root of the cwd's drive (`C:\c\x` from
/// any directory on `C:`), so those spellings give both paths.
fn windows_spellings(candidate: &str, cwd: &str) -> Option<Vec<String>> {
    let one_letter =
        |segment: &str| segment.len() == 1 && segment.as_bytes()[0].is_ascii_alphabetic();
    let mut path = candidate.replace('\\', "/");
    let mut alternate = None;
    if let Some(rest) = ["//?/", "//./", "/??/"]
        .iter()
        .find_map(|prefix| path.strip_prefix(prefix))
    {
        path = if let Some(unc) = rest
            .get(..4)
            .filter(|unc| unc.eq_ignore_ascii_case("unc/"))
            .and_then(|_| rest.get(4..))
        {
            format!("//{unc}")
        } else if valid_drive_prefix(rest) {
            rest.to_owned()
        } else {
            return None;
        };
    }
    if valid_drive_prefix(&path) && path.len() == 2 {
        path.push('/');
    }
    if let Some(rest) = path.strip_prefix("//") {
        let mut parts = rest.splitn(3, '/');
        let (server, share) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        if (server.eq_ignore_ascii_case("localhost") || server == "127.0.0.1")
            && share.len() == 2
            && share.as_bytes()[0].is_ascii_alphabetic()
            && share.as_bytes()[1] == b'$'
        {
            path = format!("{}:/{}", &share[..1], parts.next().unwrap_or(""));
        }
    } else if let Some(rest) = path.strip_prefix('/') {
        let forward = candidate.starts_with('/');
        let (first, after) = rest.split_once('/').unwrap_or((rest, ""));
        let cygdrive = after.split_once('/').unwrap_or((after, ""));
        let (root, _) = split_root(cwd)?;
        let native = format!("{}{path}", root.trim_end_matches('/'));
        if forward && one_letter(first) {
            path = format!("{}:/{after}", first.to_ascii_uppercase());
            alternate = Some(native);
        } else if forward && first.eq_ignore_ascii_case("cygdrive") && one_letter(cygdrive.0) {
            path = format!("{}:/{}", cygdrive.0.to_ascii_uppercase(), cygdrive.1);
            alternate = Some(native);
        } else {
            path = native;
        }
    } else if !is_absolute(&path) {
        path = join(cwd, &path);
    }
    let segments = |path: &str| -> Option<String> {
        let (root, tail) = split_root(path)?;
        let parts: Vec<&str> = tail.into_iter().filter_map(windows_segment).collect();
        Some(join_segments(root, &parts))
    };
    let mut spellings = vec![segments(&path)?];
    if let Some(native) = alternate {
        spellings.push(segments(&native)?);
    }
    Some(spellings)
}

/// One segment as Windows reads it ([`windows_spelling`]); `None` when nothing
/// is left of it.
fn windows_segment(segment: &str) -> Option<&str> {
    // Glob and brace syntax is expanded later; trimming here would change
    // what it expands to (`{.,}.` read as `{.,}` never reaches `..`).
    if segment.contains(['*', '?', '[', ']', '{', '}', '(', ')', '|']) {
        return Some(segment);
    }
    let name = segment.split(':').next().unwrap_or(segment);
    let unspaced = name.trim_end_matches(' ');
    if unspaced == "." || unspaced == ".." {
        return Some(unspaced);
    }
    let name = name.trim_end_matches(['.', ' ']);
    (!name.is_empty()).then_some(name)
}

fn compile(
    config: &CaptureConfig,
    marker_dir: &str,
    home_dir: Option<&str>,
) -> Result<Vec<CompiledPattern>, String> {
    if config.ignore_paths.len() > MAX_IGNORE_PATTERNS {
        return Err(format!(
            "`ignore_paths` has more than {MAX_IGNORE_PATTERNS} entries"
        ));
    }
    let marker = normalize_root(marker_dir)
        .map_err(|_| "the repository path could not be normalized".to_owned())?;
    let needs_home = config
        .ignore_paths
        .iter()
        .any(|pattern| pattern.starts_with("~/"));
    let home = if needs_home {
        Some(
            home_dir
                .and_then(|dir| normalize_root(dir).ok())
                .ok_or_else(|| {
                    "a pattern starts with '~/' but no home directory is available to expand it"
                        .to_owned()
                })?,
        )
    } else {
        None
    };
    config
        .ignore_paths
        .iter()
        .map(|source| {
            validate_glob(source)
                .map_err(|reason| format!("ignore_paths entry {source:?} {reason}"))?;
            let expanded = if let Some(rest) = source.strip_prefix("~/") {
                let home = home.as_deref().ok_or_else(|| {
                    "a pattern starts with '~/' but no home directory is available to expand it"
                        .to_owned()
                })?;
                join(home, rest)
            } else if is_absolute(source) {
                source.clone()
            } else {
                join(&marker, source)
            };
            let flavor = flavor_of(&expanded);
            // Every candidate of a Windows session is Windows-flavored, so a
            // POSIX pattern (`/c/repo/x/**`, `/x/**`) would match nothing
            // while the policy reports active: fail closed instead.
            if flavor == Flavor::Posix && flavor_of(&marker) == Flavor::Windows {
                return Err(format!(
                    "ignore_paths entry {source:?} is a POSIX absolute path in a Windows \
                     repository; spell it relative to the marker or with a drive (C:/...)"
                ));
            }
            // A Windows pattern is spelled like the candidates it is compared
            // with ([`windows_spelling`]): `\\?\C:\repo\x\**` is `C:/repo/x/**`.
            let spelled = if flavor == Flavor::Windows {
                windows_spelling(&expanded, "")
            } else {
                Some(expanded)
            };
            let path = spelled
                .as_deref()
                .and_then(normalize_segments)
                .ok_or_else(|| {
                    format!("ignore_paths entry {source:?} could not be normalized into a path")
                })?;
            Ok(CompiledPattern {
                literal_prefix: literal_prefix(&path).into(),
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

/// Diagnostic twin of the compile step [`CapturePolicy::resolve`] runs: a
/// `[capture]` table can parse as valid TOML and still fail here (too many
/// patterns, an unsupported glob character, a `..` segment, a `~/` pattern
/// with no home directory to expand it...), which is exactly the gap a
/// parse-only check (e.g. `read_capture_config_verbose`) misses. `None`
/// means this config would compile into an active policy; `Some(reason)`
/// names the first rejected pattern and why.
#[must_use]
pub fn describe_invalid_capture_config(
    config: &CaptureConfig,
    marker_dir: &str,
    home_dir: Option<&str>,
) -> Option<String> {
    if config.ignore_paths.is_empty() {
        return None;
    }
    compile(config, marker_dir, home_dir).err()
}
fn validate_glob(pattern: &str) -> Result<(), String> {
    if pattern.trim().is_empty() {
        return Err("is empty".to_owned());
    }
    if pattern.chars().count() > MAX_IGNORE_PATTERN_CHARS {
        return Err(format!(
            "is longer than {MAX_IGNORE_PATTERN_CHARS} characters"
        ));
    }
    if pattern.contains(['!', '{', '}', '[', ']', '(', ')', '|', '^', '$', '%']) {
        return Err(
            "contains an unsupported glob character (one of ! { } [ ] ( ) | ^ $ %)".to_owned(),
        );
    }
    if pattern.contains("${") {
        return Err("contains a shell-style variable expansion (${...})".to_owned());
    }
    if pattern.contains("***") {
        return Err("contains three or more consecutive '*'".to_owned());
    }
    if pattern
        .replace('\\', "/")
        .split('/')
        .any(|segment| segment == "..")
    {
        return Err("contains a '..' path segment".to_owned());
    }
    if pattern.starts_with('~') && !pattern.starts_with("~/") {
        return Err(
            "starts with '~' but not '~/' (only home-relative '~/...' is supported)".to_owned(),
        );
    }
    if is_drive_relative(pattern) {
        return Err("is drive-relative (e.g. 'C:foo' without a path separator)".to_owned());
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
/// An absolute directory (a cwd, the marker directory, home), normalized. A
/// Windows one is read as Windows opens it ([`windows_spelling`]), so it shares
/// one spelling with every candidate and pattern resolved from it: from
/// `\\?\C:\repo`, `docs/x` and `C:\repo\docs\x` must name the same file.
fn normalize_root(path: &str) -> Result<String, ()> {
    if !is_absolute(path) || is_drive_relative(path) {
        return Err(());
    }
    if flavor_of(path) == Flavor::Windows {
        // Absolute, so no cwd is consulted.
        let spelled = windows_spelling(path, "").ok_or(())?;
        return normalize_segments(&spelled).ok_or(());
    }
    normalize_segments(path).ok_or(())
}
fn normalize_segments(path: &str) -> Option<String> {
    let (root, tail) = split_root(path)?;
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
    Some(join_segments(root, &parts))
}
/// A path's root (`/`, `C:/`, `//server/share`, with either separator) and
/// the segments after it.
fn split_root(path: &str) -> Option<(String, Vec<&str>)> {
    fn tail(rest: &str) -> Vec<&str> {
        rest.split(['/', '\\']).collect()
    }
    let bytes = path.as_bytes();
    let separator = |index: usize| matches!(bytes.get(index), Some(b'/' | b'\\'));
    if separator(0) && separator(1) {
        let mut parts = path[2..].split(['/', '\\']).filter(|p| !p.is_empty());
        let server = parts.next()?;
        let share = parts.next()?;
        Some((format!("//{server}/{share}"), parts.collect()))
    } else if valid_drive_prefix(path) && separator(2) {
        Some((
            format!("{}:/", path[..1].to_ascii_uppercase()),
            tail(&path[3..]),
        ))
    } else if separator(0) {
        Some(("/".into(), tail(&path[1..])))
    } else {
        None
    }
}
fn join_segments(mut output: String, parts: &[&str]) -> String {
    if !parts.is_empty() {
        if !output.ends_with('/') {
            output.push('/');
        }
        output.push_str(&parts.join("/"));
    }
    output
}
fn glob_match(
    pattern: &str,
    candidate: &str,
    directory_base: Option<&str>,
    insensitive: bool,
) -> bool {
    // `dir/**` also names `dir` itself, and `dir` may hold globs
    // (`docs/a?r/**`), exactly as in the generated TypeScript.
    if directory_base.is_some_and(|base| glob_match(base, candidate, None, insensitive)) {
        return true;
    }
    glob_states(pattern, candidate, insensitive)
        .last()
        .copied()
        .unwrap_or(false)
}
/// Whether `pattern` matches some path strictly below `directory`. A pattern
/// prefix still alive after `directory/` can always be completed by spelling
/// out its remaining literals.
fn glob_matches_below(pattern: &str, directory: &str, insensitive: bool) -> bool {
    let directory = if directory.ends_with('/') {
        directory.to_owned()
    } else {
        format!("{directory}/")
    };
    glob_states(pattern, &directory, insensitive)
        .iter()
        .any(|alive| *alive)
}
/// For each prefix length of `pattern`, whether that prefix matches all of
/// `candidate`.
fn glob_states(pattern: &str, candidate: &str, insensitive: bool) -> Vec<bool> {
    let pattern: Vec<char> = pattern.chars().collect();
    let candidate: Vec<char> = candidate.chars().collect();
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
    previous
}
/// The whole leading segments of a normalized pattern that contain no glob.
fn literal_prefix(path: &str) -> &str {
    let Some(glob) = path.find(['*', '?']) else {
        return path;
    };
    let Some(slash) = path[..glob].rfind('/') else {
        return "";
    };
    let head = &path[..=slash];
    // Keep the root separator of `/`, `C:/`; drop it everywhere else.
    match head.strip_suffix('/') {
        Some(trimmed) if !trimmed.is_empty() && !trimmed.ends_with(':') => trimmed,
        _ => head,
    }
}
fn starts_with_chars(path: &str, prefix: &str, insensitive: bool) -> bool {
    let mut path = path.chars();
    prefix.chars().all(|expected| {
        path.next()
            .is_some_and(|c| char_equal(expected, c, insensitive))
    })
}
/// Whether a shell glob can expand to `prefix` itself or to a path below it.
/// Only the glob's first segments, as many as `prefix` has, are compared:
/// `docs/*/x.md` reaches `docs/adr`, while `*.md` in the parent never can.
fn glob_reaches(glob: &str, prefix: &str, insensitive: bool) -> bool {
    let prefix = prefix.trim_end_matches('/');
    let depth = prefix.split('/').filter(|part| !part.is_empty()).count();
    if depth == 0 {
        return true;
    }
    let mut seen = 0;
    let mut offset = 0;
    for part in glob.split('/') {
        offset += part.len();
        if !part.is_empty() {
            seen += 1;
            if seen == depth {
                return glob_match(&glob[..offset], prefix, None, insensitive);
            }
        }
        offset += 1;
    }
    false
}
/// `glob_reaches` for a glob whose `**` spans any number of segments, as in
/// Gemini CLI's `read_many_files`: once the segments before the first `**`
/// lead into `prefix` (or `prefix` lies below them), the glob can reach it.
fn globstar_reaches(glob: &str, prefix: &str, insensitive: bool) -> bool {
    let Some(star) = glob.find("**") else {
        return glob_reaches(glob, prefix, insensitive);
    };
    let head = glob[..star].rfind('/').map_or("", |slash| &glob[..slash]);
    let depth = head.split('/').filter(|part| !part.is_empty()).count();
    let mut seen = 0;
    let mut offset = 0;
    for part in prefix.split('/') {
        if seen == depth {
            break;
        }
        offset += part.len();
        if !part.is_empty() {
            seen += 1;
        }
        if seen < depth {
            offset += 1;
        }
    }
    if seen < depth {
        // `prefix` is shallower than the literal head; compare as usual.
        return glob_reaches(glob, prefix, insensitive);
    }
    glob_match(head, &prefix[..offset.min(prefix.len())], None, insensitive)
}
/// A path as a case-folding (macOS) host compares it: composed (NFC), since
/// its filesystem reads a decomposed `é` as the composed one, then lowercased
/// with the full Unicode mapping (`Ä` as `ä`), which the generated
/// TypeScript's `normalize("NFC").toLowerCase()` mirrors.
fn fold_path(path: &str) -> String {
    icu_normalizer::ComposingNormalizerBorrowed::new_nfc()
        .normalize(path)
        .to_lowercase()
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

    /// The routed `cwd` is built from these paths, so a Windows target keeps
    /// backslashes (as the harness reports every other cwd) while `..` is
    /// still collapsed; a POSIX target is unchanged. Windows CI caught the
    /// forward-slash `C:/…` form being sent as the destination cwd.
    #[test]
    fn absolute_tool_location_paths_keep_the_native_separator_style() {
        let edit = |path: &str| {
            serde_json::json!({
                "tool_name": "Edit",
                "tool_input": {"file_path": path}
            })
        };
        assert_eq!(
            absolute_tool_location_paths(
                AgentKind::ClaudeCode,
                &edit(r"C:\work\destination\sub\..\a.txt"),
                r"C:\work\source",
            ),
            Some(vec![r"C:\work\destination\a.txt".to_owned()])
        );
        assert_eq!(
            absolute_tool_location_paths(
                AgentKind::ClaudeCode,
                &edit("/work/destination/sub/../a.txt"),
                "/work/source",
            ),
            Some(vec!["/work/destination/a.txt".to_owned()])
        );
    }

    /// A tool's location is its file targets, a search tool's explicit
    /// directory, or a shell command's structured `workdir` — absolute only,
    /// and never a path parsed out of the command text (#1199).
    #[test]
    fn absolute_tool_location_paths_cover_search_dirs_and_shell_workdirs() {
        let hermes = |tool: &str, input: Value| {
            absolute_tool_location_paths(
                AgentKind::Hermes,
                &serde_json::json!({"tool_name": tool, "tool_input": input}),
                "/home/user",
            )
        };
        assert_eq!(
            hermes(
                "terminal",
                serde_json::json!({"command": "ls", "workdir": "/work/repo/./src/.."})
            ),
            Some(vec!["/work/repo".to_owned()])
        );
        assert_eq!(
            hermes(
                "search_files",
                serde_json::json!({"pattern": "x", "path": "/work/repo"})
            ),
            Some(vec!["/work/repo".to_owned()])
        );
        assert_eq!(
            hermes("read_file", serde_json::json!({"path": "/work/repo/a.rs"})),
            Some(vec!["/work/repo/a.rs".to_owned()])
        );
        for (tool, input) in [
            (
                "terminal",
                serde_json::json!({"command": "ls", "workdir": "repo"}),
            ),
            (
                "terminal",
                serde_json::json!({"command": "cat /work/repo/a.rs"}),
            ),
            ("terminal", serde_json::json!({"workdir": "/work/repo"})),
            (
                "search_files",
                serde_json::json!({"pattern": "x", "path": "."}),
            ),
            ("search_files", serde_json::json!({"pattern": "x"})),
            (
                "process",
                serde_json::json!({"action": "list", "workdir": "/work/repo"}),
            ),
        ] {
            assert_eq!(hermes(tool, input.clone()), None, "{tool} {input}");
        }
    }

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
    fn state_reports_active_for_a_valid_config() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secret/**".into()],
            }),
            "/repo",
            None,
        );
        assert_eq!(policy.state(), PolicyState::Active);
    }

    #[test]
    fn state_reports_inactive_for_an_absent_marker() {
        let policy = CapturePolicy::resolve(CaptureSource::Absent, "/repo", None);
        assert_eq!(policy.state(), PolicyState::Inactive);
    }

    #[test]
    fn state_reports_inactive_for_an_empty_ignore_paths() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: Vec::new(),
            }),
            "/repo",
            None,
        );
        assert_eq!(policy.state(), PolicyState::Inactive);
    }

    #[test]
    fn state_reports_invalid_for_a_parse_failure() {
        let policy = CapturePolicy::resolve(CaptureSource::Invalid, "/repo", None);
        assert_eq!(policy.state(), PolicyState::Invalid);
    }

    #[test]
    fn state_reports_invalid_for_a_config_that_parses_but_does_not_compile() {
        // This is exactly the gap a parse-only check misses: valid TOML,
        // valid `[capture]` shape, but a pattern `compile` itself rejects.
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secrets/{a,b}".into()],
            }),
            "/repo",
            None,
        );
        assert_eq!(policy.state(), PolicyState::Invalid);
    }

    #[test]
    fn describe_invalid_capture_config_is_none_when_it_would_compile() {
        let config = CaptureConfig {
            ignore_paths: vec!["secret/**".into()],
        };
        assert!(describe_invalid_capture_config(&config, "/repo", None).is_none());
    }

    #[test]
    fn describe_invalid_capture_config_names_the_rejected_pattern() {
        let config = CaptureConfig {
            ignore_paths: vec!["ok/**".into(), "secrets/{a,b}".into()],
        };
        let reason = describe_invalid_capture_config(&config, "/repo", None)
            .expect("a rejected glob must produce a reason");
        assert!(reason.contains("secrets/{a,b}"), "{reason}");
        assert!(reason.contains("unsupported glob character"), "{reason}");
    }

    #[test]
    fn describe_invalid_capture_config_is_none_for_an_empty_config() {
        let config = CaptureConfig {
            ignore_paths: Vec::new(),
        };
        assert!(describe_invalid_capture_config(&config, "/repo", None).is_none());
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
    /// Adversarial regression for GHSA-vh98 / security-boundaries.md row
    /// 11b: a POSIX-host candidate spelled with a leading `//` used to be
    /// classified `Flavor::Windows` purely from the string, so it matched
    /// zero POSIX `ignore_paths` patterns (flavor mismatch in `match_paths`'
    /// filter) and was captured instead of dropped. `flavor_of` cannot tell
    /// a doubled POSIX separator from a UNC root by itself; only the host
    /// (the cwd) can. This attempts the violation, proves a plain-looking
    /// control still drops normally, and proves a genuine Windows-hosted UNC
    /// candidate still matches (so the fix didn't just blanket-collapse
    /// every `//`).
    #[test]
    fn a_leading_double_slash_candidate_does_not_escape_posix_ignore_paths_via_flavor_mismatch() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secret/**".into()],
            }),
            "/repo",
            None,
        );
        // The violation attempt: on the unfixed code this normalized to a
        // Windows-flavored candidate and matched no POSIX pattern, so it
        // came back `Keep` (captured) instead of `Drop`.
        let attack =
            json!({"tool_name":"Edit","tool_input":{"file_path":"//repo/secret/token.txt"}});
        assert_eq!(
            policy
                .inspect(AgentKind::ClaudeCode, &attack, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Drop,
            "a leading `//` must not escape a POSIX host's ignore_paths"
        );
        // Legitimate control: an ordinary single-slash candidate under the
        // same pattern must keep being dropped.
        let control =
            json!({"tool_name":"Edit","tool_input":{"file_path":"/repo/secret/token.txt"}});
        assert_eq!(
            policy
                .inspect(AgentKind::ClaudeCode, &control, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Drop
        );
        // Windows-UNC control: a genuine UNC candidate on a Windows host
        // must still match a UNC pattern — the fix is host-derived, not an
        // unconditional `//` -> `/` collapse.
        let windows_policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["\\\\server\\share\\**".into()],
            }),
            "C:/",
            None,
        );
        let unc = json!({"tool_name":"Edit","tool_input":{"file_path":"//SERVER/SHARE/token.txt"}});
        assert_eq!(
            windows_policy
                .inspect(AgentKind::ClaudeCode, &unc, "C:/")
                .protocol()
                .disposition(),
            CaptureDisposition::Drop,
            "a genuine UNC candidate on a Windows host must still match"
        );
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

    /// A transcript import rebuilds each recorded tool call with
    /// `tool_call_payload`; for every agent whose hook schema `extract`
    /// supports, that rebuilt call must reach the same decision a live hook
    /// gets, or `backfill` and the run ledger would store excluded content.
    #[test]
    fn tool_call_payload_reaches_the_live_decision_for_every_supported_agent() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["docs/adr/**".into()],
            }),
            "/repo",
            None,
        );
        for agent in [
            AgentKind::AntigravityCli,
            AgentKind::Grok,
            AgentKind::Zero,
            AgentKind::ClaudeCode,
            AgentKind::CommandCode,
            AgentKind::Codex,
            AgentKind::Cursor,
            AgentKind::GeminiCli,
            AgentKind::Devin,
            AgentKind::Hermes,
            AgentKind::KiroCli,
            AgentKind::Pool,
            AgentKind::Grizzybot,
            AgentKind::Zcode,
            AgentKind::CopilotCli,
            AgentKind::Droid,
            AgentKind::Muse,
            AgentKind::CodeBuddy,
            AgentKind::QwenCode,
            AgentKind::Qoder,
            AgentKind::OpenCode,
            AgentKind::Omp,
            AgentKind::Pi,
            AgentKind::OpenClaw,
        ] {
            let decide = |name: &str, args: Option<&Value>| {
                let raw = tool_call_payload(agent, name, args, Some("call-1"));
                policy.inspect(agent, &raw, "/repo")
            };
            let excluded = decide("read", Some(&json!({"file_path": "docs/adr/1.md"})));
            assert_eq!(
                excluded.protocol().disposition(),
                CaptureDisposition::Drop,
                "{agent:?}: excluded read"
            );
            assert_eq!(excluded.call_id.as_deref(), Some("call-1"), "{agent:?}");
            let public = decide("read", Some(&json!({"file_path": "src/lib.rs"})));
            assert_eq!(
                public.protocol().disposition(),
                CaptureDisposition::Keep,
                "{agent:?}: public read"
            );
            assert_eq!(public.protocol().tool_family(), ToolFamily::File);
            let shell = decide("bash", Some(&json!({"command": "cat docs/adr/1.md"})));
            assert_eq!(
                shell.protocol().disposition(),
                CaptureDisposition::Drop,
                "{agent:?}: shell naming an excluded path"
            );
            assert_eq!(
                decide("read", None).protocol().disposition(),
                CaptureDisposition::MetadataOnly,
                "{agent:?}: unrecoverable file arguments fail closed"
            );
        }
    }

    /// Codex transcripts record the shell tool as `exec_command` with
    /// `{cmd, workdir}`; hooks receive it as `Bash`. The rebuilt call must be a
    /// shell call resolved from its `workdir`, not an unknown tool whose output
    /// is kept.
    #[test]
    fn codex_transcript_exec_command_is_inspected_as_the_hook_bash_call() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["docs/adr/**".into()],
            }),
            "/repo",
            None,
        );
        let decide = |agent: AgentKind, args: Value| {
            let raw = tool_call_payload(agent, "exec_command", Some(&args), Some("call-1"));
            policy.inspect(agent, &raw, "/repo").protocol().clone()
        };
        for args in [
            json!({"cmd": "cat adr/x.md", "workdir": "/repo/docs"}),
            json!({"cmd": "cat adr/x.md", "workdir": "docs"}),
            json!({"cmd": "cat docs/adr/x.md"}),
            json!({"cmd": ["bash", "-lc", "cat docs/adr/x.md"], "yield_time_ms": 1000}),
        ] {
            let protocol = decide(AgentKind::Codex, args.clone());
            assert_eq!(protocol.tool_family(), ToolFamily::NonFile, "{args}");
            assert_eq!(protocol.disposition(), CaptureDisposition::Drop, "{args}");
        }
        let public = decide(
            AgentKind::Codex,
            json!({"cmd": "cat adr/x.md", "workdir": "/repo/src"}),
        );
        assert_eq!(public.tool_family(), ToolFamily::NonFile);
        assert_eq!(public.disposition(), CaptureDisposition::Keep);
        let raw = tool_call_payload(
            AgentKind::Codex,
            "exec_command",
            Some(&json!({"cmd": "ls", "workdir": "/other/repo"})),
            None,
        );
        assert_eq!(
            absolute_tool_location_paths(AgentKind::Codex, &raw, "/repo"),
            Some(vec!["/other/repo".to_owned()]),
            "the workdir locates the call like a live shell workdir"
        );
        // Only Codex records its shell under this name.
        assert_eq!(
            decide(AgentKind::ClaudeCode, json!({"cmd": "cat docs/adr/x.md"})).tool_family(),
            ToolFamily::Unknown
        );
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

    /// Factory Droid's tool calls, in the 0.240.0 tool schemas (#1240): each
    /// file tool naming an ignored path is dropped or stripped while a control
    /// path is kept, search/list tools drop under an active marker, and
    /// `Execute` is matched like any shell. The same payloads under
    /// `AgentKind::Other` stay unrecognized and keep their output, which is
    /// what the Droid mapping changes.
    /// CodeBuddy Code sends Claude Code's tool names and fields (#1299), so an
    /// ignored path in a `Read`, `Write`, `Edit` or `Bash` call drops. Under
    /// `AgentKind::Other` the same calls are not recognized and keep, so the
    /// test fails if the CodeBuddy `extract` arm is removed.
    #[test]
    fn codebuddy_tool_calls_honor_capture_exclusions() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secret/**".into()],
            }),
            "/repo",
            None,
        );
        let call = |name: &str, input: Value| {
            json!({
                "hook_event_name": "PreToolUse",
                "session_id": "codebuddy-session",
                "cwd": "/repo",
                "tool_name": name,
                "tool_input": input,
                "tool_use_id": "toolu_1",
            })
        };
        let disposition =
            |agent: AgentKind, raw: &Value| policy.inspect(agent, raw, "/repo").protocol().clone();
        for (name, input, control) in [
            (
                "Read",
                json!({"file_path": "secret/a"}),
                json!({"file_path": "src/lib.rs"}),
            ),
            (
                "Write",
                json!({"file_path": "/repo/secret/a", "content": "x"}),
                json!({"file_path": "/repo/src/lib.rs", "content": "x"}),
            ),
            (
                "Edit",
                json!({"file_path": "secret/a", "old_string": "a", "new_string": "b"}),
                json!({"file_path": "src/lib.rs", "old_string": "a", "new_string": "b"}),
            ),
            (
                "Bash",
                json!({"command": "cat secret/a"}),
                json!({"command": "cat src/lib.rs"}),
            ),
        ] {
            assert_eq!(
                disposition(AgentKind::CodeBuddy, &call(name, input.clone())).disposition(),
                CaptureDisposition::Drop,
                "{name}"
            );
            assert_eq!(
                disposition(AgentKind::Other, &call(name, input)).disposition(),
                CaptureDisposition::Keep,
                "{name}: without the CodeBuddy mapping the call is not recognized"
            );
            assert_eq!(
                disposition(AgentKind::CodeBuddy, &call(name, control)).disposition(),
                CaptureDisposition::Keep,
                "{name}: control"
            );
        }
    }

    /// Bob's Claude-shaped file and shell calls must pass through the same
    /// capture exclusions. The legitimate control proves the mapping does not
    /// discard every Bob tool call.
    #[test]
    fn ibm_bob_tool_calls_honor_capture_exclusions() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secret/**".into()],
            }),
            "/repo",
            None,
        );
        let call = |path: &str| {
            json!({
                "hook_event_name": "PreToolUse",
                "session_id": "bob-session",
                "cwd": "/repo",
                "tool_name": "Read",
                "tool_input": {"file_path": path},
                "tool_use_id": "toolu_1",
            })
        };

        assert_eq!(
            policy
                .inspect(AgentKind::IbmBob, &call("secret/token"), "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Drop
        );
        assert_eq!(
            policy
                .inspect(AgentKind::IbmBob, &call("src/lib.rs"), "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Keep
        );
        assert_eq!(
            policy
                .inspect(AgentKind::Other, &call("secret/token"), "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Keep,
            "without the Bob mapping the call is not recognized"
        );
    }

    /// DSH's Claude Code hooks bridge forwards Claude Code-shaped tool
    /// payloads, so a DSH file or shell call must pass through the same
    /// capture exclusions as the harness it borrows its shape from. Without a
    /// DSH mapping `extract` returns the default extraction, `inspect` sees
    /// `ToolFamily::Unknown`, and an active `ignore_paths` keeps the call
    /// instead of dropping it.
    #[test]
    fn dsh_tool_calls_honor_capture_exclusions() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secret/**".into()],
            }),
            "/repo",
            None,
        );
        let call = |name: &str, input: Value| {
            json!({
                "hook_event_name": "PreToolUse",
                "session_id": "dsh-session",
                "cwd": "/repo",
                "tool_name": name,
                "tool_input": input,
                "tool_use_id": "toolu_1",
            })
        };

        assert_eq!(
            policy
                .inspect(
                    AgentKind::Dsh,
                    &call("Read", json!({"file_path": "secret/token"})),
                    "/repo"
                )
                .protocol()
                .disposition(),
            CaptureDisposition::Drop
        );
        assert_eq!(
            policy
                .inspect(
                    AgentKind::Dsh,
                    &call("Read", json!({"file_path": "src/lib.rs"})),
                    "/repo"
                )
                .protocol()
                .disposition(),
            CaptureDisposition::Keep
        );
        // DSH's Windows shell tool is `pwsh`; the POSIX one is `bash`.
        assert_eq!(
            policy
                .inspect(
                    AgentKind::Dsh,
                    &call("pwsh", json!({"command": "cat secret/token"})),
                    "/repo"
                )
                .protocol()
                .disposition(),
            CaptureDisposition::Drop
        );
        assert_eq!(
            policy
                .inspect(
                    AgentKind::Dsh,
                    &call("bash", json!({"command": "cat secret/token"})),
                    "/repo"
                )
                .protocol()
                .disposition(),
            CaptureDisposition::Drop
        );
        // DSH's `headless` profile mounts Code Mode, so its one-shot runs call
        // the reserved `run_code` transport with program source that builds
        // its paths at run time. Like any other inline-code tool it must fall
        // back to metadata under an active policy instead of keeping the
        // program and its output.
        assert_eq!(
            policy
                .inspect(
                    AgentKind::Dsh,
                    &call(
                        "run_code",
                        json!({
                            "code": "const r = await tools.read({file_path: 'secret/token'}); console.log(r)",
                            "description": "read an ignored file",
                        }),
                    ),
                    "/repo"
                )
                .protocol()
                .disposition(),
            CaptureDisposition::MetadataOnly
        );
        assert_eq!(
            policy
                .inspect(
                    AgentKind::Other,
                    &call("Read", json!({"file_path": "secret/token"})),
                    "/repo"
                )
                .protocol()
                .disposition(),
            CaptureDisposition::Keep,
            "without the DSH mapping the call is not recognized"
        );
    }

    #[test]
    fn droid_tool_calls_honor_capture_exclusions() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["docs/adr/**".into()],
            }),
            "/repo",
            None,
        );
        let call = |name: &str, input: Value| {
            json!({
                "hook_event_name": "PostToolUse",
                "session_id": "droid-session",
                "transcript_path": "/home/u/.factory/sessions/s.jsonl",
                "cwd": "/repo",
                "permission_mode": "auto-low",
                "tool_name": name,
                "tool_input": input,
                "tool_response": {"success": true},
            })
        };
        let disposition =
            |agent: AgentKind, raw: &Value| policy.inspect(agent, raw, "/repo").protocol().clone();
        let file_tools = [
            (
                "Read",
                json!({"file_path": "docs/adr/1.md", "offset": 0, "limit": 20}),
            ),
            (
                "Create",
                json!({"file_path": "/repo/docs/adr/2.md", "content": "secret"}),
            ),
            (
                "Edit",
                json!({"file_path": "docs/adr/1.md", "old_str": "a", "new_str": "b", "change_all": false}),
            ),
        ];
        for (name, input) in file_tools {
            let excluded = disposition(AgentKind::Droid, &call(name, input.clone()));
            assert_eq!(excluded.tool_family(), ToolFamily::File, "{name}");
            assert_eq!(excluded.disposition(), CaptureDisposition::Drop, "{name}");
            assert_eq!(
                disposition(AgentKind::Other, &call(name, input.clone())).disposition(),
                CaptureDisposition::Keep,
                "{name}: without the Droid mapping the call is not recognized"
            );
            let mut control = input;
            control["file_path"] = json!("src/lib.rs");
            let kept = disposition(AgentKind::Droid, &call(name, control));
            assert_eq!(
                kept.disposition(),
                CaptureDisposition::Keep,
                "{name}: control"
            );
        }

        // `ApplyPatch` names its files only inside the patch text, like Codex
        // `apply_patch`: under an active marker that fails closed to
        // metadata-only, and the patch headers locate the call for the
        // cross-checkout marker lookup.
        let patch = |path: &str| {
            call(
                "ApplyPatch",
                json!({"input": format!("*** Begin Patch\n*** Update File: {path}\n@@\n-a\n+b\n*** End Patch")}),
            )
        };
        let excluded = disposition(AgentKind::Droid, &patch("docs/adr/1.md"));
        assert_eq!(excluded.tool_family(), ToolFamily::File);
        assert_eq!(excluded.disposition(), CaptureDisposition::MetadataOnly);
        let codex = tool_call_payload(
            AgentKind::Codex,
            "apply_patch",
            Some(
                &json!({"input": "*** Begin Patch\n*** Update File: docs/adr/1.md\n*** End Patch"}),
            ),
            None,
        );
        assert_eq!(
            policy
                .inspect(AgentKind::Codex, &codex, "/repo")
                .protocol()
                .disposition(),
            excluded.disposition(),
            "ApplyPatch is decided like Codex apply_patch"
        );
        assert_eq!(
            disposition(AgentKind::Other, &patch("docs/adr/1.md")).disposition(),
            CaptureDisposition::Keep
        );
        assert_eq!(
            capture_lookup_locations(AgentKind::Droid, &patch("../other/x.rs"), "/repo", None),
            Some(vec!["/other/x.rs".to_owned()])
        );
        assert_eq!(
            CapturePolicy::inactive()
                .inspect(AgentKind::Droid, &patch("docs/adr/1.md"), "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Keep,
            "control: no marker keeps the patch"
        );

        for (name, input) in [
            ("LS", json!({"directory_path": "docs/adr"})),
            ("Grep", json!({"pattern": "KEY", "path": "docs/adr"})),
            (
                "Glob",
                json!({"patterns": ["docs/adr/*.md"], "folder": "/repo"}),
            ),
        ] {
            let decision = disposition(AgentKind::Droid, &call(name, input.clone()));
            assert_eq!(decision.tool_family(), ToolFamily::SearchList, "{name}");
            assert_eq!(decision.disposition(), CaptureDisposition::Drop, "{name}");
            assert_eq!(
                disposition(AgentKind::Other, &call(name, input)).disposition(),
                CaptureDisposition::Keep,
                "{name}: without the Droid mapping the call is not recognized"
            );
        }
        // Droid's directory and glob arguments locate the call in another
        // checkout.
        let elsewhere = |name: &str, input: Value| {
            capture_lookup_locations(AgentKind::Droid, &call(name, input), "/repo", None)
        };
        assert_eq!(
            elsewhere("LS", json!({"directory_path": "/other/docs"})),
            Some(vec!["/other/docs".to_owned()])
        );
        assert_eq!(
            elsewhere("Glob", json!({"patterns": "src/*.rs", "folder": "/other"})),
            Some(vec!["/other".to_owned(), "/other/src/*.rs".to_owned()])
        );
        assert_eq!(
            elsewhere(
                "Grep",
                json!({"pattern": "KEY", "path": "/other", "glob_pattern": "a/*.env"})
            ),
            Some(vec!["/other".to_owned(), "/other/a/*.env".to_owned()])
        );
        assert_eq!(
            absolute_tool_location_paths(
                AgentKind::Droid,
                &call("LS", json!({"directory_path": "/other/docs"})),
                "/repo"
            ),
            Some(vec!["/other/docs".to_owned()])
        );

        let shell =
            |command: &str| call("Execute", json!({"command": command, "riskLevel": "low"}));
        let excluded = disposition(AgentKind::Droid, &shell("cat docs/adr/1.md"));
        assert_eq!(excluded.tool_family(), ToolFamily::NonFile);
        assert_eq!(excluded.disposition(), CaptureDisposition::Drop);
        assert_eq!(
            disposition(AgentKind::Droid, &shell("cat src/lib.rs")).disposition(),
            CaptureDisposition::Keep
        );
        assert_eq!(
            disposition(AgentKind::Other, &shell("cat docs/adr/1.md")).disposition(),
            CaptureDisposition::Keep
        );
    }

    /// Droid hook payloads carry no tool-call id, so the drop list and the
    /// observation metadata read the tool name alone (#1240).
    #[test]
    fn droid_tool_payload_fields_have_no_call_id() {
        let raw = json!({
            "tool_name": "Execute",
            "tool_input": {"command": "ls"},
            "tool_use_id": "would-be-ignored"
        });
        assert_eq!(
            tool_payload_fields(AgentKind::Droid, &raw),
            Some(("Execute", None))
        );
        let metadata = tool_observation_metadata(AgentKind::Droid, &raw, true).unwrap();
        assert_eq!(metadata.tool_family, ToolFamily::NonFile);
        assert_eq!(metadata.tool_call_id, None);
        let no_input = json!({"tool_name": "Read"});
        assert!(tool_observation_metadata(AgentKind::Droid, &no_input, true).is_none());
        assert!(tool_observation_metadata(AgentKind::Droid, &no_input, false).is_some());
        for (tool, expected) in [
            ("Read", ToolFamily::File),
            ("Create", ToolFamily::File),
            ("Edit", ToolFamily::File),
            ("ApplyPatch", ToolFamily::File),
            ("LS", ToolFamily::SearchList),
            ("Grep", ToolFamily::SearchList),
            ("Glob", ToolFamily::SearchList),
            ("Execute", ToolFamily::NonFile),
        ] {
            assert_eq!(agent_family(AgentKind::Droid, tool), expected, "{tool}");
        }
        // The Droid-only names keep their classification for other agents.
        assert_eq!(
            agent_family(AgentKind::ClaudeCode, "ApplyPatch"),
            ToolFamily::Unknown
        );
        assert_eq!(
            agent_family(AgentKind::ClaudeCode, "Create"),
            ToolFamily::Unknown
        );
    }

    #[test]
    fn cline_nested_tool_payloads_are_closed_and_classified() {
        let pre = json!({
            "tool_call": {
                "id": "call-1",
                "name": "read_file",
                "input": {"path": "src/lib.rs"}
            }
        });
        assert_eq!(
            tool_payload_fields(AgentKind::Cline, &pre),
            Some(("read_file", Some("call-1")))
        );
        let metadata = tool_observation_metadata(AgentKind::Cline, &pre, true).unwrap();
        assert_eq!(metadata.tool_family, ToolFamily::File);
        assert_eq!(metadata.tool_call_id.as_deref(), Some("call-1"));

        let success = json!({
            "tool_result": {"id": "call-1", "name": "read_file", "output": "ok"}
        });
        let failure = json!({
            "tool_result": {"id": "call-1", "name": "read_file", "error": "denied"}
        });
        assert_eq!(
            tool_observation_outcome(AgentKind::Cline, &success),
            ToolOutcome::Success
        );
        assert_eq!(
            tool_observation_outcome(AgentKind::Cline, &failure),
            ToolOutcome::Error
        );
        assert_eq!(tool_payload_fields(AgentKind::Other, &pre), None);
    }

    /// Meta Muse Code's tool calls, in the shapes captured offline from Muse
    /// Code 1.4.7 hooks: `tool_input.path` is relative as the model wrote it
    /// and resolves lexically against `cwd`. Each file tool naming an ignored
    /// path, relative or absolute, is dropped while a control path is kept;
    /// `search` drops under an active marker; `bash` and `monitor` are matched
    /// like any shell from their `workdir`; `bash_input` reads back an earlier
    /// command's output. The same payloads under `AgentKind::Other` stay
    /// unrecognized and keep their output, which is what the Muse mapping
    /// changes.
    #[test]
    fn muse_tool_calls_honor_capture_exclusions() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["docs/adr/**".into(), ".agents/memory/secret/**".into()],
            }),
            "/repo",
            None,
        );
        let call = |name: &str, input: Value| {
            json!({
                "hook_event_name": "PostToolUse",
                "session_id": "01a13669-dc61-7a72-b5ec-ad2f300ffd26",
                "transcript_path": null,
                "cwd": "/repo",
                "model": "muse-wonder-1.0",
                "model_provider": "meta",
                "permission_mode": "bypassPermissions",
                "turn_id": "ccfe4a87-c654-49f5-b5e9-473015c7a90b",
                "tool_name": name,
                "tool_input": input,
                "tool_use_id": "call_30a33793ad9c",
                "tool_response": {"filePath": "/repo/x", "structuredPatch": []},
            })
        };
        let disposition =
            |agent: AgentKind, raw: &Value| policy.inspect(agent, raw, "/repo").protocol().clone();
        let file_tools = [
            ("read_file", json!({"path": "docs/adr/1.md"})),
            (
                "read_file",
                json!({"path": "/repo/docs/adr/1.md", "offset": 1}),
            ),
            (
                "write_file",
                json!({"path": "docs/adr/2.md", "content": "secret"}),
            ),
            (
                "write_file",
                json!({"path": "/repo/docs/adr/2.md", "content": "secret"}),
            ),
            (
                "edit_file",
                json!({"path": "docs/adr/1.md", "find": "a", "replace": "b"}),
            ),
            (
                "edit_file",
                json!({"path": "/repo/docs/adr/1.md", "find": "a", "replace": "b"}),
            ),
            (
                "add_memory",
                json!({"path": "secret/n.md", "scope": "project", "content": "x"}),
            ),
            (
                "edit_memory",
                json!({"path": "secret/n.md", "scope": "project", "old_str": "a", "new_str": "b"}),
            ),
            (
                "read_memory",
                json!({"path": "secret/n.md", "scope": "project"}),
            ),
        ];
        for (name, input) in file_tools {
            let excluded = disposition(AgentKind::Muse, &call(name, input.clone()));
            assert_eq!(excluded.tool_family(), ToolFamily::File, "{name} {input}");
            assert_eq!(
                excluded.disposition(),
                CaptureDisposition::Drop,
                "{name} {input}"
            );
            assert_eq!(
                disposition(AgentKind::Other, &call(name, input.clone())).disposition(),
                CaptureDisposition::Keep,
                "{name}: without the Muse mapping the call is not recognized"
            );
            let mut control = input;
            control["path"] = json!("src/lib.rs");
            let kept = disposition(AgentKind::Muse, &call(name, control));
            assert_eq!(
                kept.disposition(),
                CaptureDisposition::Keep,
                "{name}: control"
            );
        }
        // A memory path is relative to its scope's directory, not to `cwd`:
        // `docs/adr/1.md` in the project scope is `.agents/memory/docs/adr/1.md`.
        assert_eq!(
            disposition(
                AgentKind::Muse,
                &call(
                    "read_memory",
                    json!({"path": "docs/adr/1.md", "scope": "project"})
                )
            )
            .disposition(),
            CaptureDisposition::Keep
        );
        // The personal scopes live outside the repository, where no candidate
        // reaches, so they fail closed under an active marker.
        for scope in [json!("personal"), json!("personal_project"), Value::Null] {
            let decision = disposition(
                AgentKind::Muse,
                &call("add_memory", json!({"path": "n.md", "scope": scope})),
            );
            assert_eq!(decision.tool_family(), ToolFamily::File, "{scope}");
            assert_eq!(
                decision.disposition(),
                CaptureDisposition::MetadataOnly,
                "{scope}"
            );
            assert_eq!(
                CapturePolicy::inactive()
                    .inspect(
                        AgentKind::Muse,
                        &call("add_memory", json!({"path": "n.md", "scope": scope})),
                        "/repo"
                    )
                    .protocol()
                    .disposition(),
                CaptureDisposition::Keep,
                "control: no marker keeps the memory call"
            );
        }

        // `apply_patch` (the `openai-apply-patch` preset) carries a Codex
        // envelope in `patch`.
        let patch = |path: &str| {
            call(
                "apply_patch",
                json!({"patch": format!("*** Begin Patch\n*** Update File: {path}\n@@\n-a\n+b\n*** End Patch")}),
            )
        };
        let excluded = disposition(AgentKind::Muse, &patch("docs/adr/1.md"));
        assert_eq!(excluded.tool_family(), ToolFamily::File);
        assert_eq!(excluded.disposition(), CaptureDisposition::MetadataOnly);
        assert_eq!(
            capture_lookup_locations(AgentKind::Muse, &patch("../other/x.rs"), "/repo", None),
            Some(vec!["/other/x.rs".to_owned()])
        );

        // `search` names globs in `paths` and filters with `glob`.
        let search = call("search", json!({"pattern": "beta", "paths": ["*.txt"]}));
        let decision = disposition(AgentKind::Muse, &search);
        assert_eq!(decision.tool_family(), ToolFamily::SearchList);
        assert_eq!(decision.disposition(), CaptureDisposition::Drop);
        assert_eq!(
            capture_lookup_locations(
                AgentKind::Muse,
                &call(
                    "search",
                    json!({"pattern": "KEY", "paths": ["/other"], "glob": "a/*.env"})
                ),
                "/repo",
                None
            ),
            Some(vec!["/other".to_owned(), "/other/a/*.env".to_owned()])
        );

        let shell = |name: &str, command: &str, workdir: &str| {
            call(
                name,
                json!({"command": command, "description": "x", "workdir": workdir}),
            )
        };
        for name in ["bash", "monitor"] {
            let excluded = disposition(AgentKind::Muse, &shell(name, "cat adr/1.md", "/repo/docs"));
            assert_eq!(excluded.tool_family(), ToolFamily::NonFile, "{name}");
            assert_eq!(excluded.disposition(), CaptureDisposition::Drop, "{name}");
            assert_eq!(
                disposition(AgentKind::Muse, &shell(name, "cat notes.txt", "/repo")).disposition(),
                CaptureDisposition::Keep,
                "{name}: control"
            );
        }
        assert_eq!(
            disposition(
                AgentKind::Other,
                &shell("monitor", "cat adr/1.md", "/repo/docs")
            )
            .disposition(),
            CaptureDisposition::Keep,
            "monitor: without the Muse mapping the call is not recognized"
        );
        let input = call("bash_input", json!({"session_id": "exec-1", "chars": "q"}));
        assert_eq!(
            disposition(AgentKind::Muse, &input).disposition(),
            CaptureDisposition::MetadataOnly,
            "bash_input reads back an earlier command's output"
        );
        assert_eq!(
            CapturePolicy::inactive()
                .inspect(AgentKind::Muse, &input, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Keep
        );
        assert_eq!(
            disposition(AgentKind::Other, &input).disposition(),
            CaptureDisposition::Keep
        );
    }

    /// Muse hook payloads carry the provider call id in `tool_use_id`, so the
    /// drop list and the observation metadata read both fields, and a
    /// `PostToolUseFailure` `error` proves an error outcome.
    #[test]
    fn muse_tool_payload_fields_carry_the_call_id() {
        let raw = json!({
            "tool_name": "write_file",
            "tool_input": {"path": "notes.txt", "content": "alpha\n"},
            "tool_use_id": "call_30a33793ad9c"
        });
        assert_eq!(
            tool_payload_fields(AgentKind::Muse, &raw),
            Some(("write_file", Some("call_30a33793ad9c")))
        );
        assert_eq!(tool_payload_fields(AgentKind::Other, &raw), None);
        let metadata = tool_observation_metadata(AgentKind::Muse, &raw, true).unwrap();
        assert_eq!(metadata.tool_family, ToolFamily::File);
        assert_eq!(metadata.tool_call_id.as_deref(), Some("call_30a33793ad9c"));
        let no_input = json!({"tool_name": "read_file"});
        assert!(tool_observation_metadata(AgentKind::Muse, &no_input, true).is_none());
        assert!(tool_observation_metadata(AgentKind::Muse, &no_input, false).is_some());
        let failure = json!({
            "hook_event_name": "PostToolUseFailure",
            "tool_name": "read_file",
            "tool_input": {"path": "missing.txt"},
            "tool_use_id": "call_a2a05a20bbb2",
            "error": "tool failed: No such file or directory (os error 2)",
            "is_interrupt": false,
            "duration_ms": 0
        });
        assert_eq!(
            tool_observation_outcome(AgentKind::Muse, &failure),
            ToolOutcome::Error
        );
        assert_eq!(
            tool_observation_outcome(AgentKind::Muse, &raw),
            ToolOutcome::Unknown
        );
        for (tool, expected) in [
            ("read_file", ToolFamily::File),
            ("write_file", ToolFamily::File),
            ("edit_file", ToolFamily::File),
            ("apply_patch", ToolFamily::File),
            ("read_memory", ToolFamily::File),
            ("add_memory", ToolFamily::File),
            ("edit_memory", ToolFamily::File),
            ("search", ToolFamily::SearchList),
            ("bash", ToolFamily::NonFile),
            ("shell", ToolFamily::NonFile),
            ("monitor", ToolFamily::NonFile),
            ("bash_input", ToolFamily::Unknown),
            ("web_fetch", ToolFamily::NonFile),
            ("web_search", ToolFamily::NonFile),
        ] {
            assert_eq!(agent_family(AgentKind::Muse, tool), expected, "{tool}");
        }
        // The Muse-only names keep their classification for other agents.
        for tool in ["add_memory", "monitor"] {
            assert_eq!(
                agent_family(AgentKind::ClaudeCode, tool),
                ToolFamily::Unknown,
                "{tool}"
            );
        }
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
    fn grizzybot_tool_shape_is_closed_and_unknown_agents_stay_unsupported() {
        let raw = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "web_search",
            "tool_input": {"query": "swift"},
            "session_id": "gb-session",
            "cwd": "/bot/home"
        });
        let metadata = tool_observation_metadata(AgentKind::Grizzybot, &raw, true).unwrap();
        assert_eq!(metadata.tool_family, ToolFamily::NonFile);
        // A PreToolUse without an input is not a proven shape.
        let no_input = json!({"tool_name": "read_file"});
        assert!(tool_observation_metadata(AgentKind::Grizzybot, &no_input, true).is_none());
        assert!(tool_observation_metadata(AgentKind::Grizzybot, &no_input, false).is_some());
    }

    /// Claude Code's documented `PostToolUseFailure` payload proves an error;
    /// its success payload proves nothing.
    #[test]
    fn claude_code_post_tool_use_failure_error_maps_to_error_outcome() {
        let failure = json!({
            "hook_event_name": "PostToolUseFailure",
            "tool_name": "Bash",
            "tool_input": {"command": "npm test", "description": "Run test suite"},
            "tool_use_id": "toolu_01ABC123",
            "error": "Exit code 1
        Error: Cannot find module 'express'",
            "is_interrupt": false,
            "duration_ms": 4187
        });
        assert_eq!(
            tool_observation_outcome(AgentKind::ClaudeCode, &failure),
            ToolOutcome::Error
        );
        // An abort is still a call that did not succeed.
        let interrupted = json!({
            "hook_event_name": "PostToolUseFailure",
            "tool_name": "Bash",
            "tool_input": {"command": "sleep 100"},
            "error": "Command aborted",
            "is_interrupt": true
        });
        assert_eq!(
            tool_observation_outcome(AgentKind::ClaudeCode, &interrupted),
            ToolOutcome::Error
        );
        // Control: a success and an empty `error` stay unknown.
        let success = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "npm test"},
            "tool_response": {"stdout": "ok", "stderr": "", "interrupted": false}
        });
        assert_eq!(
            tool_observation_outcome(AgentKind::ClaudeCode, &success),
            ToolOutcome::Unknown
        );
        assert_eq!(
            tool_observation_outcome(
                AgentKind::ClaudeCode,
                &json!({"tool_name": "Bash", "error": ""})
            ),
            ToolOutcome::Unknown
        );
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
    fn copilot_cli_outcome_reads_only_proven_result_types() {
        let outcome = |raw: Value| tool_observation_outcome(AgentKind::CopilotCli, &raw);
        let with_type = |result_type: &str| {
            json!({"tool_name": "bash", "tool_result": {
                "result_type": result_type, "text_result_for_llm": "x"
            }})
        };
        assert_eq!(outcome(with_type("success")), ToolOutcome::Success);
        assert_eq!(outcome(with_type("failure")), ToolOutcome::Error);
        assert_eq!(outcome(with_type("error")), ToolOutcome::Error);
        // A denied or rejected call is not a tool failure, and a future type
        // proves nothing.
        assert_eq!(outcome(with_type("denied")), ToolOutcome::Unknown);
        // PostToolUseFailure (aliased to PostToolUse) carries `error` and no
        // `tool_result`.
        assert_eq!(
            outcome(json!({"tool_name": "bash", "error": "permission denied"})),
            ToolOutcome::Error
        );
        assert_eq!(outcome(json!({"tool_name": "bash"})), ToolOutcome::Unknown);
    }

    #[test]
    fn copilot_cli_tool_metadata_uses_the_claude_compatible_keys() {
        let pre = json!({
            "tool_name": "edit",
            "tool_input": {"path": "src/lib.rs"},
        });
        assert!(tool_observation_metadata(AgentKind::CopilotCli, &pre, true).is_some());
        // PreToolUse without a proven input shape stays metadata-less.
        let no_input = json!({"tool_name": "edit"});
        assert!(tool_observation_metadata(AgentKind::CopilotCli, &no_input, true).is_none());
    }

    #[test]
    fn qwen_code_tool_metadata_uses_documented_claude_shaped_fields() {
        let pre = json!({
            "tool_name": "write_file",
            "tool_input": {"file_path": "src/lib.rs", "content": "x"},
            "tool_use_id": "toolu_qwen",
        });
        let metadata = tool_observation_metadata(AgentKind::QwenCode, &pre, true).unwrap();
        assert_eq!(metadata.tool_family, ToolFamily::File);
        assert_eq!(metadata.tool_call_id.as_deref(), Some("toolu_qwen"));
        assert_eq!(
            tool_observation_outcome(
                AgentKind::QwenCode,
                &json!({"tool_name": "write_file", "error": "disk full"})
            ),
            ToolOutcome::Error
        );
        assert_eq!(
            tool_observation_outcome(AgentKind::QwenCode, &json!({"tool_name": "write_file"})),
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
    fn shell_policy() -> CapturePolicy {
        CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["docs/adr/**".into(), "~/notes/**".into()],
            }),
            "/repo",
            Some("/home/me"),
        )
    }
    fn bash(command: &str) -> Value {
        json!({"tool_name": "Bash", "tool_input": {"command": command}, "tool_use_id": "call-1"})
    }

    /// zsh, and bash with `globstar`, recurse on `**`, so a shell glob whose
    /// `**` can descend into an ignored directory drops the event; a `**`
    /// rooted elsewhere is still kept (control).
    #[test]
    fn recursive_shell_globs_cannot_reach_ignored_paths() {
        let policy = shell_policy();
        for command in ["cat **/*.md", "cat **/0001.md", "cat docs/**/0001.md"] {
            let decision = policy.inspect(AgentKind::ClaudeCode, &bash(command), "/repo");
            assert_eq!(
                decision.protocol().disposition(),
                CaptureDisposition::Drop,
                "{command}"
            );
        }
        let decision = policy.inspect(AgentKind::ClaudeCode, &bash("cat src/**/main.rs"), "/repo");
        assert_eq!(decision.protocol().disposition(), CaptureDisposition::Keep);
    }

    /// The drop/keep tables live in the shared fixture so the generated
    /// TypeScript matcher runs the very same vectors (#961).
    /// The shared Windows-spelling vectors. Adversarial: every Git Bash,
    /// MSYS2, Cygwin, rooted, device, administrative-share, trailing
    /// dot/space and alternate-data-stream spelling of an ignored file in a
    /// Windows session drops, file tool or shell. Controls: the same names on
    /// another drive or share, a longer name, and a POSIX session (where a
    /// trailing dot, a colon or `/c/...` is part of the name) keep.
    #[test]
    fn windows_spelling_fixture_vectors() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/capture-policy.json")).unwrap();
        let sections = ["windows", "windows_posix_controls"]
            .iter()
            .map(|name| (*name, &fixture[*name]))
            .chain(
                fixture["windows_scopes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|section| ("windows_scopes", section)),
            );
        for (name, section) in sections {
            let cwd = section["cwd"].as_str().unwrap();
            let config = CaptureConfig {
                ignore_paths: section["ignore_paths"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|pattern| pattern.as_str().unwrap().to_owned())
                    .collect(),
            };
            let policy =
                CapturePolicy::resolve_for_host(CaptureSource::Parsed(&config), cwd, None, false);
            let shell = section["shell"].as_array().cloned().unwrap_or_default();
            let files = section["files"].as_array().unwrap().iter().map(|vector| {
                (
                    json!({"tool": "edit", "args": {"path": vector["path"]}}),
                    vector,
                )
            });
            let commands = shell.iter().map(|vector| {
                let payload = vector.get("payload").cloned().unwrap_or_else(
                    || json!({"tool": "bash", "args": {"command": vector["command"]}}),
                );
                (payload, vector)
            });
            for (payload, vector) in files.chain(commands) {
                let decision = policy.inspect(AgentKind::OpenCode, &payload, cwd);
                assert_eq!(
                    serde_json::to_value(decision.protocol().disposition()).unwrap(),
                    vector["disposition"],
                    "{name} ({cwd}): {vector}"
                );
            }
        }
    }

    /// A Windows marker's POSIX absolute pattern would match no Windows
    /// candidate while reporting active, so it is rejected; the same pattern
    /// in a POSIX marker, and a drive or relative pattern in a Windows one,
    /// stay active.
    #[test]
    fn windows_marker_rejects_posix_absolute_patterns() {
        let config = |pattern: &str| CaptureConfig {
            ignore_paths: vec![pattern.to_owned()],
        };
        for pattern in ["/c/repo/docs/private/**", "/docs/private/**"] {
            let reason = describe_invalid_capture_config(&config(pattern), r"C:\repo", None);
            assert!(
                reason.as_deref().is_some_and(|r| r.contains("POSIX")),
                "{pattern}: {reason:?}"
            );
            let policy =
                CapturePolicy::resolve(CaptureSource::Parsed(&config(pattern)), r"C:\repo", None);
            assert_eq!(policy.state(), PolicyState::Invalid, "{pattern}");
            let posix =
                CapturePolicy::resolve(CaptureSource::Parsed(&config(pattern)), "/repo", None);
            assert_eq!(posix.state(), PolicyState::Active, "{pattern}");
        }
        for pattern in [
            "C:/repo/docs/private/**",
            r"docs\private\**",
            "docs/private/**",
        ] {
            let policy =
                CapturePolicy::resolve(CaptureSource::Parsed(&config(pattern)), r"C:\repo", None);
            assert_eq!(policy.state(), PolicyState::Active, "{pattern}");
        }
    }

    /// Server re-inspection compiles a never-match sentinel as Active. The
    /// sentinel must share the cwd's path flavor: a POSIX absolute one is
    /// Invalid on a Windows repository (GHSA-gcpc-88hm-3xqr), which is how
    /// #1250 over-stripped kept events.
    #[test]
    fn inspection_sentinel_matches_cwd_flavor() {
        const POSIX: &str = "/__ai_memory_capture_server_inspection_never_match__";
        const WINDOWS: &str = "C:/__ai_memory_capture_server_inspection_never_match__";
        let config = |pattern: &str| CaptureConfig {
            ignore_paths: vec![pattern.to_owned()],
        };
        let state = |pattern: &str, cwd: &str| {
            CapturePolicy::resolve(CaptureSource::Parsed(&config(pattern)), cwd, None).state()
        };
        assert_eq!(inspection_sentinel(r"C:\repo"), WINDOWS);
        assert_eq!(inspection_sentinel(r"\\server\share\repo"), WINDOWS);
        assert_eq!(inspection_sentinel("/repo"), POSIX);
        assert_eq!(inspection_sentinel("/c/repo"), POSIX);
        assert_eq!(state(POSIX, r"C:\repo"), PolicyState::Invalid);
        assert!(
            describe_invalid_capture_config(&config(POSIX), r"C:\repo", None)
                .as_deref()
                .is_some_and(|reason| reason.contains("POSIX"))
        );
        assert_eq!(state(WINDOWS, r"C:\repo"), PolicyState::Active);
        assert_eq!(state(POSIX, "/repo"), PolicyState::Active);
        assert_eq!(state(POSIX, "/c/repo"), PolicyState::Active);
        assert_eq!(state(WINDOWS, r"\\server\share\repo"), PolicyState::Active);
    }

    /// Native and generated TypeScript classify every tool name alike; the
    /// TypeScript lists are generated from these constants.
    #[test]
    fn tool_family_fixture_vectors() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/capture-policy.json")).unwrap();
        for vector in fixture["tool_families"].as_array().unwrap() {
            let name = vector["name"].as_str().unwrap();
            assert_eq!(
                serde_json::to_value(family(name)).unwrap(),
                vector["family"],
                "{name}"
            );
        }
    }

    /// Adversarial: a path of thousands of `*` segments used to cost a
    /// quadratic number of allocating glob intersections outside any budget
    /// (~400 ms for one, 14 s for 32). The check is linear, charged to the
    /// match budget, and a call past the budget fails closed. Controls: a
    /// real spool path is still found, and one long path or 32 short ones
    /// keep.
    #[test]
    fn output_spool_work_is_linear_and_budgeted() {
        let path = format!("/tmp/{}x", "*/".repeat(2040));
        let mut work = 0;
        assert_eq!(output_spool(&path, &mut work), Ok(false));
        assert!(
            work <= 64 * path.len(),
            "spool work {work} for {} chars",
            path.len()
        );
        assert_eq!(
            output_spool("/home/u/.claude/projects/p/s/tool-results/a.txt", &mut 0),
            Ok(true)
        );
        assert_eq!(output_spool(&path, &mut MAX_MATCH_WORK.clone()), Err(()));
        // A two-character pattern keeps the path match itself under budget
        // (natively and in the generated TypeScript), so only the spool check
        // can run out.
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["/s".into()],
            }),
            "/repo",
            None,
        );
        let read = |paths: &[String]| {
            policy
                .inspect(
                    AgentKind::ClaudeCode,
                    &json!({"tool_name": "Read", "tool_input": {"paths": paths}}),
                    "/repo",
                )
                .protocol()
                .disposition()
        };
        let long: Vec<String> = (0..32)
            .map(|i| format!("/tmp/{i}/{}x", "*/".repeat(2000)))
            .collect();
        assert_eq!(read(&long), CaptureDisposition::MetadataOnly);
        assert_eq!(read(&long[..1]), CaptureDisposition::Keep);
        let short: Vec<String> = (0..32).map(|i| format!("/tmp/{i}/x")).collect();
        assert_eq!(read(&short), CaptureDisposition::Keep);
    }

    /// A command longer than [`MAX_SHELL_COMMAND_CHARS`] is not split: it is
    /// metadata-only under a policy and its marker lookup is unprovable.
    /// Controls: one character shorter keeps and is looked up, and without a
    /// policy nothing changes.
    #[test]
    fn an_oversized_shell_command_is_unprovable() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secret/**".into()],
            }),
            "/repo",
            None,
        );
        let at_limit = format!("cat public.md{}", " ".repeat(MAX_SHELL_COMMAND_CHARS - 13));
        let over = format!("{at_limit} ");
        let bash = |command: &str| json!({"tool_name": "Bash", "tool_input": {"command": command}});
        let decide = |policy: &CapturePolicy, raw: &Value| {
            policy
                .inspect(AgentKind::ClaudeCode, raw, "/repo")
                .protocol()
                .disposition()
        };
        assert_eq!(decide(&policy, &bash(&at_limit)), CaptureDisposition::Keep);
        assert_eq!(
            decide(&policy, &bash(&over)),
            CaptureDisposition::MetadataOnly
        );
        let argv =
            json!({"tool_name": "shell", "tool_input": {"command": ["bash", "-lc", at_limit]}});
        assert_eq!(
            policy
                .inspect(AgentKind::Codex, &argv, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::MetadataOnly
        );
        assert_eq!(
            decide(&CapturePolicy::inactive(), &bash(&over)),
            CaptureDisposition::Keep
        );
        assert!(
            capture_lookup_locations(AgentKind::ClaudeCode, &bash(&over), "/repo", None).is_none()
        );
        assert!(
            capture_lookup_locations(AgentKind::ClaudeCode, &bash(&at_limit), "/repo", None)
                .is_some()
        );
    }

    /// Hermes `execute_code` runs program source, which names its paths at
    /// run time: metadata-only under an active or broken policy, untouched
    /// without one. Control: a browser tool keeps.
    #[test]
    fn inline_code_tools_are_unprovable_under_a_policy() {
        let raw = json!({"tool_name": "execute_code", "tool_input": {"code": "print(open('secret/a.txt').read())"}});
        let active = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["secret/**".into()],
            }),
            "/repo",
            None,
        );
        let invalid = CapturePolicy::resolve(CaptureSource::Invalid, "/repo", None);
        for (policy, expected) in [
            (&active, CaptureDisposition::MetadataOnly),
            (&invalid, CaptureDisposition::MetadataOnly),
            (&CapturePolicy::inactive(), CaptureDisposition::Keep),
        ] {
            let decision = policy.inspect(AgentKind::Hermes, &raw, "/repo");
            assert_eq!(decision.protocol().disposition(), expected);
            assert_eq!(decision.protocol().tool_family(), ToolFamily::NonFile);
        }
        let browse = json!({"tool_name": "browser_navigate", "tool_input": {"url": "https://example.com/secret/a"}});
        assert_eq!(
            active
                .inspect(AgentKind::Hermes, &browse, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Keep
        );
    }

    #[test]
    fn shell_fixture_vectors() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/capture-policy.json")).unwrap();
        let shell = &fixture["shell"];
        let ignore_paths = shell["ignore_paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pattern| pattern.as_str().unwrap().to_owned())
            .collect();
        let config = CaptureConfig { ignore_paths };
        // The host's case folding is pinned per vector, so the table means
        // the same on every CI OS.
        let policy = |fold_case| {
            CapturePolicy::resolve_for_host(
                CaptureSource::Parsed(&config),
                "/repo",
                Some("/home/me"),
                fold_case,
            )
        };
        for vector in shell["vectors"].as_array().unwrap() {
            let policy = policy(vector["fold_case"].as_bool().unwrap_or(false));
            let payload = vector
                .get("payload")
                .cloned()
                .unwrap_or_else(|| json!({"tool": "bash", "args": {"command": vector["command"]}}));
            let payload: Value =
                serde_json::from_str(&payload.to_string().replace("{root}", "/repo")).unwrap();
            let cwd = vector["cwd"]
                .as_str()
                .map_or_else(|| "/repo".to_owned(), |sub| format!("/repo/{sub}"));
            let decision = policy.inspect(AgentKind::OpenCode, &payload, &cwd);
            let protocol = decision.protocol();
            assert_eq!(
                serde_json::to_value(protocol.disposition()).unwrap(),
                vector["disposition"],
                "vector: {vector}"
            );
            assert_eq!(
                protocol.tool_family(),
                ToolFamily::NonFile,
                "vector: {vector}"
            );
            // Unchanged protocol fields keep old and new servers agreeing.
            assert_eq!(protocol.path_count(), 0);
            assert_eq!(protocol.extraction_state(), ExtractionState::Extracted);
        }
    }

    #[test]
    fn shell_tool_shapes_of_every_adapter_honor_exclusions() {
        let policy = shell_policy();
        for (agent, raw) in [
            (AgentKind::ClaudeCode, bash("cat docs/adr/x.md")),
            (
                AgentKind::Codex,
                json!({"tool_name": "shell", "tool_input": {"command": ["bash", "-lc", "cat docs/adr/x.md"]}}),
            ),
            (
                AgentKind::Codex,
                json!({"tool_name": "exec_command", "tool_input": {"cmd": "cat docs/adr/x.md"}}),
            ),
            (
                AgentKind::KiroCli,
                json!({"tool_name": "execute_bash", "tool_input": {"command": "cat docs/adr/x.md"}}),
            ),
            (
                AgentKind::Hermes,
                json!({"tool_name": "terminal", "tool_input": {"command": "cat docs/adr/x.md"}}),
            ),
            (
                AgentKind::OpenCode,
                json!({"tool": "bash", "args": {"command": "cat docs/adr/x.md"}}),
            ),
            (
                AgentKind::OpenClaw,
                json!({"tool": "exec", "args": {"command": "cat 0001.md", "workdir": "docs/adr"}}),
            ),
            (
                AgentKind::Devin,
                json!({"tool_name": "exec", "tool_input": {"command": "cat docs/adr/x.md"}}),
            ),
            // DSH's Windows shell tool beside its POSIX `bash`.
            (
                AgentKind::Dsh,
                json!({"tool_name": "pwsh", "tool_input": {"command": "cat docs/adr/x.md"}}),
            ),
            (
                AgentKind::Dsh,
                json!({"tool_name": "bash", "tool_input": {"command": "cat docs/adr/x.md"}}),
            ),
        ] {
            let decision = policy.inspect(agent, &raw, "/repo");
            if decision.protocol().tool_family() == ToolFamily::NonFile {
                assert_eq!(
                    decision.protocol().disposition(),
                    CaptureDisposition::Drop,
                    "raw: {raw}"
                );
            } else {
                // `exec_command` is not a recognized shell tool name; it keeps
                // today's unknown-tool behavior rather than guessing.
                assert_eq!(raw["tool_name"], "exec_command", "raw: {raw}");
                assert_eq!(decision.protocol().tool_family(), ToolFamily::Unknown);
            }
        }
    }

    /// Gemini CLI's `run_shell_command` and `read_many_files` fell to the
    /// unknown family, so `ignore_paths` never read their arguments and the
    /// raw output reached the spool. `read_many_files` takes globs that Gemini
    /// expands case-insensitively with a recursive `**`; any one that names or
    /// can expand into an ignored path drops the call, and glob syntax the
    /// matcher cannot evaluate fails closed.
    #[test]
    fn gemini_cli_shell_and_read_many_files_honor_ignored_paths() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["docs/adr/**".into()],
            }),
            "/repo",
            None,
        );
        let gemini = |tool: &str, input: Value| {
            json!({"hook_event_name": "AfterTool", "tool_name": tool, "tool_input": input,
                "tool_response": {"llmContent": "SENTINEL_CONTENT"}})
        };
        let read_many = |include: &[&str]| gemini("read_many_files", json!({"include": include}));
        let shell = |input: Value| gemini("run_shell_command", input);
        for (raw, expected, family) in [
            (
                read_many(&["docs/adr/x.md"]),
                CaptureDisposition::Drop,
                ToolFamily::File,
            ),
            (
                read_many(&["README.md", "docs/adr/x.md"]),
                CaptureDisposition::Drop,
                ToolFamily::File,
            ),
            (
                read_many(&["docs/adr"]),
                CaptureDisposition::Drop,
                ToolFamily::File,
            ),
            (
                read_many(&["docs/*/x.md"]),
                CaptureDisposition::Drop,
                ToolFamily::File,
            ),
            (
                read_many(&["**/*.md"]),
                CaptureDisposition::Drop,
                ToolFamily::File,
            ),
            (
                read_many(&["docs/**"]),
                CaptureDisposition::Drop,
                ToolFamily::File,
            ),
            (
                read_many(&["/repo/**/x.md"]),
                CaptureDisposition::Drop,
                ToolFamily::File,
            ),
            (
                read_many(&["Docs/ADR/x.md"]),
                CaptureDisposition::Drop,
                ToolFamily::File,
            ),
            (
                read_many(&[r"docs\adr\x.md"]),
                CaptureDisposition::Drop,
                ToolFamily::File,
            ),
            (
                read_many(&[
                    "src/**/*.rs",
                    "*.md",
                    "docs/guide.md",
                    "docs/adr-notes/x.md",
                ]),
                CaptureDisposition::Keep,
                ToolFamily::File,
            ),
            (
                read_many(&["src/**"]),
                CaptureDisposition::Keep,
                ToolFamily::File,
            ),
            (
                read_many(&["/elsewhere/**/*.md"]),
                CaptureDisposition::Keep,
                ToolFamily::File,
            ),
            // `exclude` cannot be trusted to subtract an ignored path.
            (
                gemini(
                    "read_many_files",
                    json!({"include": ["docs/**"], "exclude": ["docs/adr/**"]}),
                ),
                CaptureDisposition::Drop,
                ToolFamily::File,
            ),
            // Pre-rename spelling of the same tool.
            (
                gemini("read_many_files", json!({"paths": ["docs/adr/x.md"]})),
                CaptureDisposition::Drop,
                ToolFamily::File,
            ),
            (
                gemini(
                    "read_many_files",
                    json!({"paths": ["README.md"], "include": ["docs/adr/*"]}),
                ),
                CaptureDisposition::Drop,
                ToolFamily::File,
            ),
            // Unprovable globs and shapes fail closed to metadata only.
            (
                read_many(&["docs/ad[r]/x.md"]),
                CaptureDisposition::MetadataOnly,
                ToolFamily::File,
            ),
            (
                read_many(&["docs/{adr,x}/*.md"]),
                CaptureDisposition::MetadataOnly,
                ToolFamily::File,
            ),
            (
                read_many(&["README.md", "!(x)"]),
                CaptureDisposition::MetadataOnly,
                ToolFamily::File,
            ),
            (
                read_many(&[]),
                CaptureDisposition::MetadataOnly,
                ToolFamily::File,
            ),
            (
                gemini("read_many_files", json!({"include": ["README.md", 7]})),
                CaptureDisposition::MetadataOnly,
                ToolFamily::File,
            ),
            (
                shell(json!({"command": "cat docs/adr/x.md"})),
                CaptureDisposition::Drop,
                ToolFamily::NonFile,
            ),
            (
                shell(json!({"command": "cat x.md", "dir_path": "docs/adr"})),
                CaptureDisposition::Drop,
                ToolFamily::NonFile,
            ),
            (
                shell(json!({"command": "cat x.md", "dir_path": "/repo/docs/adr"})),
                CaptureDisposition::Drop,
                ToolFamily::NonFile,
            ),
            (
                shell(json!({"command": "cat x.md", "directory": "docs/adr"})),
                CaptureDisposition::Drop,
                ToolFamily::NonFile,
            ),
            (
                shell(json!({"command": "cat README.md", "description": "show"})),
                CaptureDisposition::Keep,
                ToolFamily::NonFile,
            ),
            (
                shell(json!({"command": "cat x.md", "dir_path": "src"})),
                CaptureDisposition::Keep,
                ToolFamily::NonFile,
            ),
            (
                gemini("list_directory", json!({"dir_path": "docs"})),
                CaptureDisposition::Drop,
                ToolFamily::SearchList,
            ),
            (
                gemini("search_file_content", json!({"pattern": "x"})),
                CaptureDisposition::Drop,
                ToolFamily::SearchList,
            ),
            (
                gemini("grep_search", json!({"pattern": "x"})),
                CaptureDisposition::Drop,
                ToolFamily::SearchList,
            ),
            (
                gemini("glob", json!({"pattern": "**/*.md"})),
                CaptureDisposition::Drop,
                ToolFamily::SearchList,
            ),
            (
                gemini("read_file", json!({"file_path": "docs/adr/x.md"})),
                CaptureDisposition::Drop,
                ToolFamily::File,
            ),
            (
                gemini(
                    "write_file",
                    json!({"file_path": "docs/adr/x.md", "content": "x"}),
                ),
                CaptureDisposition::Drop,
                ToolFamily::File,
            ),
            (
                gemini(
                    "replace",
                    json!({"file_path": "docs/adr/x.md", "old_string": "a", "new_string": "b"}),
                ),
                CaptureDisposition::Drop,
                ToolFamily::File,
            ),
            (
                gemini("read_file", json!({"file_path": "README.md"})),
                CaptureDisposition::Keep,
                ToolFamily::File,
            ),
        ] {
            let decision = policy.inspect(AgentKind::GeminiCli, &raw, "/repo");
            assert_eq!(decision.protocol().disposition(), expected, "raw: {raw}");
            assert_eq!(decision.protocol().tool_family(), family, "raw: {raw}");
        }

        // Windows host: Gemini normalizes `\` and ignores case like the drive.
        let windows = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["docs/adr/**".into()],
            }),
            r"C:\repo",
            None,
        );
        for (include, expected) in [
            (r"DOCS\adr\*.md", CaptureDisposition::Drop),
            (r"C:\repo\**\x.md", CaptureDisposition::Drop),
            (r"src\**\*.rs", CaptureDisposition::Keep),
        ] {
            let raw = read_many(&[include]);
            let decision = windows.inspect(AgentKind::GeminiCli, &raw, r"C:\repo");
            assert_eq!(decision.protocol().disposition(), expected, "{include}");
        }

        // An absolute `dir_path` locates the shell call for rerouting, exactly
        // like a `workdir`; a relative one cannot prove a checkout.
        assert_eq!(
            absolute_tool_location_paths(
                AgentKind::GeminiCli,
                &shell(json!({"command": "ls", "dir_path": "/other/repo"})),
                "/repo"
            ),
            Some(vec!["/other/repo".to_owned()])
        );
        assert_eq!(
            absolute_tool_location_paths(
                AgentKind::GeminiCli,
                &shell(json!({"command": "ls", "dir_path": "sub"})),
                "/repo"
            ),
            None
        );
    }

    /// Gemini CLI reads a directory entry of `read_many_files` as
    /// `entry/**`, so an entry naming an ancestor of an ignored directory
    /// (`docs`, `.`) reads the ignored files. The shared vectors attempt it
    /// through both payload shapes; `src` and `README.md` are the controls.
    #[test]
    fn read_many_files_directory_entries_cannot_reach_ignored_paths() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/capture-policy.json")).unwrap();
        let section = &fixture["read_many_files_directories"];
        let ignore_paths = section["ignore_paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pattern| pattern.as_str().unwrap().to_owned())
            .collect();
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig { ignore_paths }),
            "/repo",
            None,
        );
        for vector in section["vectors"].as_array().unwrap() {
            let include = &vector["include"];
            for (agent, raw) in [
                (
                    AgentKind::GeminiCli,
                    json!({"tool_name": "read_many_files", "tool_input": {"include": include}}),
                ),
                (
                    AgentKind::QwenCode,
                    json!({"tool_name": "read_many_files", "tool_input": {"include": include}}),
                ),
                (
                    AgentKind::OpenCode,
                    json!({"tool": "read_many_files", "args": {"include": include}}),
                ),
            ] {
                let protocol = policy.inspect(agent, &raw, "/repo").protocol().clone();
                assert_eq!(
                    serde_json::to_value(protocol.disposition()).unwrap(),
                    vector["disposition"],
                    "{raw}"
                );
                assert_eq!(protocol.tool_family(), ToolFamily::File);
            }
        }
        assert!(glob_matches_below("/repo/docs/adr/**", "/repo", false));
        assert!(glob_matches_below("/**", "/", false));
        assert!(glob_matches_below("/repo/**/*.md", "/repo/src", false));
        assert!(!glob_matches_below("/repo/*.md", "/repo/README.md", false));
        assert!(!glob_matches_below("/repo/docs/adr/**", "/repo/src", false));
        assert!(glob_matches_below("C:/Docs/**", "c:/docs", true));
    }

    /// Tools that read back an earlier command's output are stripped to
    /// metadata under an active or invalid policy, since the command they
    /// read may have been dropped; with no policy they are untouched.
    #[test]
    fn output_reader_tools_are_metadata_only_only_under_a_policy() {
        let reader = json!({
            "tool_name": "BashOutput", "tool_input": {"bash_id": "b1"},
            "tool_response": "PRIVATE_BODY_SENTINEL",
        });
        let active = shell_policy();
        let invalid = CapturePolicy::resolve(CaptureSource::Invalid, "/repo", None);
        let inactive = CapturePolicy::resolve(CaptureSource::Absent, "/repo", None);
        for policy in [&active, &invalid] {
            let decision = policy.inspect(AgentKind::ClaudeCode, &reader, "/repo");
            assert_eq!(
                decision.protocol().disposition(),
                CaptureDisposition::MetadataOnly
            );
            let body = metadata_only_body(Some("s"), Some("/repo"), &decision).to_string();
            assert!(!body.contains("PRIVATE_BODY_SENTINEL"), "{body}");
        }
        assert_eq!(
            inactive
                .inspect(AgentKind::ClaudeCode, &reader, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Keep
        );
        // Control: an unrelated unknown tool keeps today's behavior.
        let other = json!({"tool_name": "FutureTool", "tool_input": {"bash_id": "b1"}});
        assert_eq!(
            active
                .inspect(AgentKind::ClaudeCode, &other, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Keep
        );
    }

    /// A file a harness saved a tool's output to (Claude Code's
    /// `tool-results/` and background `tasks/*.output`, OpenCode's
    /// `tool-output/`, Pi's `pi-bash-*.log`) holds an earlier command's
    /// output, which may come from a dropped event, so reading it back is
    /// metadata-only under an active or invalid policy, like `BashOutput`.
    /// Without a policy it is kept, and an ignored path still drops.
    #[test]
    fn reading_a_harness_output_spool_is_metadata_only_under_a_policy() {
        let active = shell_policy();
        let invalid = CapturePolicy::resolve(CaptureSource::Invalid, "/repo", None);
        let inactive = CapturePolicy::resolve(CaptureSource::Absent, "/repo", None);
        let read = |path: &str| {
            json!({
                "tool_name": "Read", "tool_input": {"file_path": path},
                "tool_response": "PRIVATE_BODY_SENTINEL",
            })
        };
        let disposition = |policy: &CapturePolicy, raw: &Value| {
            policy
                .inspect(AgentKind::ClaudeCode, raw, "/repo")
                .protocol()
                .disposition()
        };
        for spool in [
            "/tmp/claude-1000/-repo/0f1e/tasks/bvljb9mrz.output",
            "/private/var/folders/x/T/claude-501/-repo/0f1e/tasks/b1.output",
            "/home/me/.claude/projects/-repo/0f1e/tool-results/toolu_1.txt",
            "C:\\Users\\me\\.claude\\projects\\-repo\\0f1e\\tool-results\\t.txt",
            "/home/me/.local/share/opencode/tool-output/tool_10a9b7111001",
            "/tmp/pi-bash-0a1b2c3d4e5f6a7b.log",
        ] {
            for policy in [&active, &invalid] {
                let decision = policy.inspect(AgentKind::ClaudeCode, &read(spool), "/repo");
                assert_eq!(
                    decision.protocol().disposition(),
                    CaptureDisposition::MetadataOnly,
                    "{spool}"
                );
                let body = metadata_only_body(Some("s"), Some("/repo"), &decision).to_string();
                assert!(!body.contains("PRIVATE_BODY_SENTINEL"), "{body}");
            }
            assert_eq!(
                disposition(&inactive, &read(spool)),
                CaptureDisposition::Keep
            );
            assert_eq!(
                disposition(&active, &bash(&format!("cat '{spool}'"))),
                CaptureDisposition::MetadataOnly,
                "{spool}"
            );
        }
        // Controls: the same names elsewhere are ordinary files.
        for path in [
            "/tmp/claude-1000/-repo/0f1e/scratchpad/notes.txt",
            "/repo/tasks/b1.output",
            "/repo/projects/x/0f1e/tool-results/a.txt",
            "/repo/tool-output/tool_1",
            "/repo/src/pi-bash.rs",
        ] {
            assert_eq!(
                disposition(&active, &read(path)),
                CaptureDisposition::Keep,
                "{path}"
            );
            assert_eq!(
                disposition(&active, &bash(&format!("cat {path}"))),
                CaptureDisposition::Keep,
                "{path}"
            );
        }
        assert_eq!(
            disposition(&active, &bash("cat docs/adr/1.md /tmp/pi-bash-1.log")),
            CaptureDisposition::Drop
        );
    }

    /// A Windows session trims trailing dots from plain segments only: a
    /// glob or brace segment is expanded later, and `{.,}.` trimmed to
    /// `{.,}` would never reach `..`, hiding the sibling checkout's marker
    /// (the Windows-only miss the hosted runner caught). Control: a plain
    /// trailing-dot segment is still read as Windows reads it.
    #[test]
    fn windows_lookup_locations_keep_glob_segments_whole() {
        let cwd = r"C:\work\b";
        let locations =
            |raw: &Value| capture_lookup_locations(AgentKind::ClaudeCode, raw, cwd, None);
        assert_eq!(
            locations(&bash("cat {.,}./a/secrets.env")),
            Some(vec![
                r"C:\work\b".to_owned(),
                r"C:\work\b\{.,}.\a\secrets.env".to_owned(),
            ])
        );
        assert_eq!(
            locations(&bash("cat a.\\secrets.env")),
            Some(vec![
                r"C:\work\b".to_owned(),
                r"C:\work\b\a\secrets.env".to_owned()
            ])
        );
    }

    /// The locations a call names, for the hook to find each one's marker:
    /// every file target, and every shell argument that spells a path,
    /// resolved from the tool's directory, plus that directory.
    #[test]
    fn capture_lookup_locations_name_every_path_a_call_spells() {
        let locations = |raw: &Value| {
            capture_lookup_locations(AgentKind::ClaudeCode, raw, "/work/b", Some("/home/me"))
        };
        assert_eq!(
            locations(
                &json!({"tool_name": "Read", "tool_input": {"file_path": "../a/secrets.env"}})
            ),
            Some(vec!["/work/a/secrets.env".to_owned()])
        );
        assert_eq!(
            locations(&bash(
                "cat ../a/secrets.env /abs/c/x ~/notes/y \"~/lit/z\" secrets.env --in=../d/e"
            )),
            Some(vec![
                "/work/b".to_owned(),
                "/work/a/secrets.env".to_owned(),
                "/abs/c/x".to_owned(),
                "/home/me/notes/y".to_owned(),
                "/work/b/~/lit/z".to_owned(),
                "/work/d/e".to_owned(),
            ])
        );
        assert_eq!(
            locations(&json!({
                "tool_name": "Bash",
                "tool_input": {"command": "cat secrets.env", "workdir": "/work/a"},
            })),
            Some(vec!["/work/a".to_owned()])
        );
        assert_eq!(
            capture_lookup_locations(
                AgentKind::ClaudeCode,
                &json!({"tool_name": "Read", "tool_input": {"file_path": "C:\\a\\x.md"}}),
                "C:\\b",
                None,
            ),
            Some(vec!["C:\\a\\x.md".to_owned()])
        );
        // A search/list tool names its root and the path-like globs below it;
        // a grep pattern is a regular expression and names nothing.
        assert_eq!(
            locations(
                &json!({"tool_name": "Grep", "tool_input": {"path": "../a", "pattern": "x/y", "glob": "*.env"}})
            ),
            Some(vec!["/work/a".to_owned()])
        );
        assert_eq!(
            locations(&json!({"tool_name": "Glob", "tool_input": {"pattern": "../a/*.env"}})),
            Some(vec!["/work/a/*.env".to_owned()])
        );
        assert_eq!(
            locations(
                &json!({"tool_name": "grep_search", "tool_input": {"dir_path": "/work/a", "include": ["src/*.rs", "*.md"]}})
            ),
            Some(vec!["/work/a".to_owned(), "/work/a/src/*.rs".to_owned()])
        );
        assert_eq!(
            locations(&json!({"tool_name": "Grep", "tool_input": {"pattern": "x"}})),
            Some(Vec::new())
        );
        // An argument that is not a string, a file tool without readable
        // targets, and a target that cannot be resolved prove nothing.
        for raw in [
            json!({"tool_name": "Grep", "tool_input": {"path": 7}}),
            json!({"tool_name": "Glob", "tool_input": {"pattern": 7}}),
            json!({"tool_name": "Read", "tool_input": {}}),
            json!({"tool_name": "Read", "tool_input": {"file_path": "C:x"}}),
            json!({"tool_name": "apply_patch", "tool_input": {"command": "no headers"}}),
        ] {
            assert_eq!(locations(&raw), None, "{raw}");
        }
        // An `apply_patch` names the files its headers name.
        assert_eq!(
            locations(
                &json!({"tool_name": "apply_patch", "tool_input": {"command": "*** Begin Patch\n*** Update File: ../a/secrets.env\n*** Move to: c.md\n*** End Patch"}})
            ),
            Some(vec![
                "/work/a/secrets.env".to_owned(),
                "/work/b/c.md".to_owned()
            ])
        );
        // Unknown tools stay with the event's own policy.
        assert_eq!(
            locations(&json!({"tool_name": "FutureTool", "tool_input": {"path": "../a/x"}})),
            Some(Vec::new())
        );
        let many = (0..=MAX_LOOKUP_LOCATIONS)
            .map(|index| format!("d{index}/x"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(locations(&bash(&format!("cat {many}"))), None);
    }

    /// A glob location is followed from its literal root; syntax that can
    /// leave the root (a brace spanning a separator, a `..` or `~`
    /// alternative) cannot be followed.
    #[test]
    fn glob_walk_follows_every_segment_a_glob_could_pass_through() {
        assert_eq!(glob_walk("/work/a/secrets.env"), GlobWalk::Literal);
        assert_eq!(glob_walk("/work/x/${HOME:-a}/*"), GlobWalk::Literal);
        assert_eq!(
            glob_walk("/work/*/secrets.env"),
            GlobWalk::Walk {
                root: "/work".into(),
                steps: vec![
                    GlobStep::Matching("*".into()),
                    GlobStep::Named("secrets.env".into())
                ],
            }
        );
        assert_eq!(
            glob_walk("/work/{a,b}/[xy]/**/z/*"),
            GlobWalk::Walk {
                root: "/work".into(),
                steps: vec![GlobStep::Any, GlobStep::Any, GlobStep::AnyDepth],
            }
        );
        assert_eq!(
            glob_walk("C:\\work\\a?\\x"),
            GlobWalk::Walk {
                root: "C:\\work".into(),
                steps: vec![GlobStep::Matching("a?".into()), GlobStep::Named("x".into())],
            }
        );
        for location in [
            "/work/{a/x,b}/secrets.env",
            "/work/b/{..,c}/secrets.env",
            "/work/b/{~,c}/x",
            "/work/b/@(..|c)/x",
            "/work/b/(a/x)/y",
        ] {
            assert_eq!(glob_walk(location), GlobWalk::Unprovable, "{location}");
        }
        // A brace sequence and a backup-file glob stay walkable.
        assert!(matches!(
            glob_walk("/work/b/ad{1..3}/x"),
            GlobWalk::Walk { .. }
        ));
        assert!(matches!(glob_walk("/work/b/src/*~"), GlobWalk::Walk { .. }));
        assert!(glob_segment_matches("SEC*", "secrets"));
        assert!(!glob_segment_matches("*.env", "src"));
    }

    /// Adversarial: a brace expansion can build a `..` or a separator out of
    /// pieces that are not one, which leaves
    /// the walk's root just like a literal `..` alternative. Controls: braces
    /// whose words stay below the root, sequences included, are walked.
    #[test]
    fn a_brace_that_builds_a_parent_step_cannot_be_walked() {
        for location in [
            "/work/b/.{.,x}/a/secrets.env",
            "/work/b/{.,}./a/secrets.env",
            "/work/b/.{.,.}/a/secrets.env",
            "/work/b/.{-..0}/a/secrets.env",
            "/work/b/{x,.{.,y}}/a/secrets.env",
            "/work/b/{a,{~,c}}/x",
        ] {
            assert_eq!(glob_walk(location), GlobWalk::Unprovable, "{location}");
        }
        for location in [
            "/work/{a,c}/x",
            "/work/b/.{git,hub}/x",
            "/work/b/v{1..3}/x",
            "/work/b/{a..c}.d/x",
            "/work/b/{.,x}/y",
        ] {
            assert!(
                matches!(glob_walk(location), GlobWalk::Walk { .. }),
                "{location}"
            );
        }
        assert_eq!(
            brace_alternatives(".{-..0}"),
            Some(vec![".-".into(), "..".into(), "./".into(), ".0".into()])
        );
        assert_eq!(
            brace_alternatives("a{1..5..2}{x,y}"),
            Some(
                ["a1x", "a1y", "a3x", "a3y", "a5x", "a5y"]
                    .map(String::from)
                    .to_vec()
            )
        );
        assert_eq!(brace_alternatives("{a}b{}"), Some(vec!["{a}b{}".into()]));
        assert_eq!(brace_alternatives("{1..1000}"), None);
        assert_eq!(brace_alternatives(&"{a,b}".repeat(7)), None);
    }

    /// Regression (v2.6.3 kept these): a brace expansion with more words than
    /// [`brace_alternatives`] lists is walked like any glob, so inside the
    /// own checkout it costs nothing; only one whose pieces could build a
    /// separator, `~`, a group, `..` or a dot-glob that names the parent
    /// cannot be. Adversarial: such oversized braces stay unprovable. A
    /// directory alternative an older bash expands to `..` (`.{*,x}/`) is
    /// unprovable at any size.
    #[test]
    fn an_oversized_brace_expansion_is_walked_unless_its_pieces_could_leave() {
        for location in [
            "/work/b/test/file{1..200}.txt",
            "/work/b/build/{1..100}/out",
            "/work/b/logs/app.{1..99}.log",
            "/work/b/data/{a..h}{1..9}.csv",
            "/work/b/{A..Z}{a..z}/x",
            "/work/b/x{1..9}{1..9}.{a,b,c}/y",
            "/work/b/.{1..100}/y",
            "/work/b/.cache{1..100}/y",
            "/work/b/v{001..100}/y",
            "/work/b/{-100..100}/y",
        ] {
            assert!(
                matches!(glob_walk(location), GlobWalk::Walk { .. }),
                "{location}"
            );
        }
        for location in [
            "/work/b/.{-..~}/a/secrets.env",
            "/work/b/.{!..z}/a/x",
            "/work/b/.{.,a}{,}{,}{,}{,}{,}{,}/a/x",
            "/work/b/{.,}{.,}{,}{,}{,}{,}{,}/a/x",
            "/work/b/.{,a,b}{,c}{,d}{,e}{,f}{,g}*/a/x",
            "/work/b/{1..100}~/x",
        ] {
            assert_eq!(glob_walk(location), GlobWalk::Unprovable, "{location}");
        }
        for location in [
            "/work/b/.{*,x}/a/x",
            "/work/b/.{?,x}/a/x",
            "/work/b/x/.{[.],y}/z",
        ] {
            assert_eq!(glob_walk(location), GlobWalk::Unprovable, "{location}");
        }
        // A final segment names files, which never step out.
        assert!(matches!(glob_walk("/work/b/.{*,x}"), GlobWalk::Walk { .. }));
        for segment in [
            "file{1..200}.txt",
            "app.{1..99}.log",
            "{a..h}{1..9}.csv",
            "{x,y}{1..99}",
        ] {
            assert!(!brace_overflow_could_leave(segment), "{segment}");
        }
        for segment in [
            ".{-..~}",
            "{,.}{.,}{,}{,}{,}{,}{,}",
            ".{,a}{,b}{,c}{,d}{,e}{,f}{,g}*",
            "{0..z}",
        ] {
            assert!(brace_overflow_could_leave(segment), "{segment}");
        }
    }

    /// Adversarial: a `**` matches no directory as well as many, so a `..`
    /// after it steps back from the directory before it (`sub/**/../x` reads
    /// `x`), which a lexical `..` that only removes the `**` misses. Both the
    /// event's own policy and the marker lookup read every such reading.
    /// Controls: a `**` without a later `..`, and a `..` that steps back
    /// over an ordinary segment, read as before.
    #[test]
    fn a_parent_step_after_a_globstar_is_read_both_ways() {
        let paths = |candidate: &str| {
            normalize_readings(candidate, "/repo")
                .unwrap()
                .unwrap()
                .into_iter()
                .map(|reading| reading.path)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            paths("sub/**/../secrets.env"),
            ["/repo/secrets.env", "/repo/sub/**/secrets.env"]
        );
        assert_eq!(paths("./**/../../a/x"), ["/a/x", "/repo/**/a/x"]);
        assert_eq!(paths("src/**/*.rs"), ["/repo/src/**/*.rs"]);
        assert_eq!(paths("src/x/../y"), ["/repo/src/y"]);
        assert_eq!(
            normalize_readings(&"**/../".repeat(20), "/repo").map(|r| r.is_some()),
            Err(())
        );
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["docs/adr/**".into(), "secrets.env".into()],
            }),
            "/repo",
            None,
        );
        let read_many = |include: &[&str]| json!({"tool_name": "read_many_files", "tool_input": {"include": include}});
        let decide = |raw: &Value| {
            policy
                .inspect(AgentKind::GeminiCli, raw, "/repo")
                .protocol()
                .disposition()
        };
        assert_eq!(
            decide(&bash("cat sub/**/../secrets.env")),
            CaptureDisposition::Drop
        );
        assert_eq!(
            decide(&bash("cat src/**/../../repo/secrets.env")),
            CaptureDisposition::Drop
        );
        assert_eq!(
            decide(&read_many(&["src/**/../docs/adr"])),
            CaptureDisposition::Drop
        );
        assert_eq!(
            decide(&bash(&format!("cat {}x", "**/../".repeat(20)))),
            CaptureDisposition::Drop
        );
        assert_eq!(
            decide(&bash("cat sub/**/../README.md")),
            CaptureDisposition::Keep
        );
        assert_eq!(
            decide(&read_many(&["src/**/../lib"])),
            CaptureDisposition::Keep
        );
        let locations = |raw: &Value| {
            capture_lookup_locations(AgentKind::ClaudeCode, raw, "/work/b", None).unwrap()
        };
        for raw in [
            bash("cat ./**/../a/secrets.env"),
            json!({"tool_name": "Glob", "tool_input": {"pattern": "./**/../a/secrets.env"}}),
            json!({"tool_name": "Grep", "tool_input": {"pattern": "KEY", "glob": "**/../a/secrets.env"}}),
        ] {
            assert!(
                locations(&raw).contains(&"/work/a/secrets.env".to_owned()),
                "{raw}"
            );
        }
    }

    /// Adversarial: bash before 5.2 lets a leading-dot pattern match `..`, so
    /// a directory segment that can (`.` then `*`, `?` or a class admitting
    /// `.`) is read as `..` too, by the event's own matcher and the lookup.
    /// Controls: a final segment, a literal dot directory and a dot glob that
    /// cannot match `..` read as before.
    #[test]
    fn a_dot_glob_directory_that_can_match_the_parent_is_read_as_it() {
        for segment in [
            ".*",
            ".?",
            ".[.]",
            ".[!x]",
            ".[^x]",
            ".[--0]",
            ".**",
            ".[[:punct:]]",
            ".[[:print:]]",
            ".[[:graph:]]",
            ".[[:ascii:]]",
            ".[![:alpha:]]",
            ".[^[:space:]]",
            ".[[=.=]]",
            ".[[.period.]]",
            ".[[:nope:]]",
            ".@(.)",
            ".?(.)",
            ".*(.)",
            ".+(.)",
            ".!(x)",
            ".@(x|.)",
            ".@(?)",
            ".+(@(.))",
        ] {
            assert!(could_name_parent(segment), "{segment}");
        }
        for segment in [
            ".g*",
            ".[!.]",
            ".[x]",
            "*",
            "?",
            "..",
            ".",
            ".config",
            ".?x",
            ".[[:alpha:]]",
            ".[[:digit:]]",
            ".[![:punct:]]",
            ".[[=x=]]",
            ".@(x)",
            ".+(x|y)",
            ".!(.)",
            ".@(.)x",
            "x@(.)",
        ] {
            assert!(!could_name_parent(segment), "{segment}");
        }
        let paths = |candidate: &str| {
            normalize_readings(candidate, "/work/b")
                .unwrap()
                .unwrap()
                .into_iter()
                .map(|reading| reading.path)
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(".*/a/x"), ["/work/a/x", "/work/b/.*/a/x"]);
        assert_eq!(paths("./.?/a/x"), ["/work/a/x", "/work/b/.?/a/x"]);
        assert_eq!(paths("src/.*"), ["/work/b/src/.*"]);
        assert_eq!(paths(".github/*.yml"), ["/work/b/.github/*.yml"]);
        assert_eq!(
            normalize_readings(&".*/".repeat(20), "/work/b").map(|r| r.is_some()),
            Err(())
        );
        let locations = |command: &str| {
            capture_lookup_locations(AgentKind::ClaudeCode, &bash(command), "/work/b", None)
                .unwrap()
        };
        for command in [
            "cat .*/a/secrets.env",
            "cat ./.?/a/secrets.env",
            "cat .[[:punct:]]/a/secrets.env",
            "cat .@(.)/a/secrets.env",
            "cat .?(.)/a/secrets.env",
            "cat .*(.)/a/secrets.env",
            "cat .+(.)/a/secrets.env",
        ] {
            assert!(
                locations(command).contains(&"/work/a/secrets.env".to_owned()),
                "{command}"
            );
        }
        for command in [
            "ls .*",
            "cat .config/x",
            "cat .github/*.yml",
            "cat .[[:alpha:]]*/x",
            "ls .[[:alnum:]]*",
        ] {
            assert!(
                locations(command).iter().all(|l| l.starts_with("/work/b")),
                "{command}"
            );
        }
    }

    /// Adversarial: a zsh glob group splits into subshell tokens and an
    /// ANSI-C string keeps its numeric escapes, so neither spelled the other
    /// checkout's path for the marker lookup. The lookup reads the words as the shell expands them.
    /// Controls: a subshell, a command substitution and an array
    /// assignment add no such word.
    #[test]
    fn glob_groups_and_ansi_c_escapes_reach_the_marker_lookup() {
        let locations = |command: &str| {
            capture_lookup_locations(AgentKind::ClaudeCode, &bash(command), "/work/b", None)
                .unwrap()
        };
        for (command, location) in [
            ("cat ../(a)/secrets.env", "/work/(a)/secrets.env"),
            ("cat ../(a|c)/secrets.env", "/work/(a|c)/secrets.env"),
            ("cat (../a|x)/secrets.env", "/work/b/(../a|x)/secrets.env"),
            ("cat ..$'\\x2f'a/secrets.env", "/work/a/secrets.env"),
            ("cat $'\\056\\056/a'/secrets.env", "/work/a/secrets.env"),
            ("cat ..$'\\u002f'a/secrets.env", "/work/a/secrets.env"),
            (
                "bash -c \"cat ../(a)/secrets.env\"",
                "/work/(a)/secrets.env",
            ),
            // A double-quoted script loses the backslash before `$`, so the
            // shell it runs reads an ANSI-C string.
            (
                "bash -c \"cat \\$'\\\\x2e\\\\x2e'/a/secrets.env\"",
                "/work/a/secrets.env",
            ),
            (
                "eval \"cat \\$'\\\\x2e\\\\x2e'/a/secrets.env\"",
                "/work/a/secrets.env",
            ),
            (
                "sh -c \"bash -c \\\"cat \\\\\\$'\\\\x2e\\\\x2e'/a/secrets.env\\\"\"",
                "/work/a/secrets.env",
            ),
        ] {
            assert!(
                locations(command).contains(&location.to_owned()),
                "{command}: {:?}",
                locations(command)
            );
        }
        assert_eq!(
            glob_walk("/work/(a)/secrets.env"),
            GlobWalk::Walk {
                root: "/work".into(),
                steps: vec![GlobStep::Any, GlobStep::Named("secrets.env".into())],
            }
        );
        assert_eq!(
            glob_walk("/work/b/(../a|x)/secrets.env"),
            GlobWalk::Unprovable
        );
        for command in ["(cd .. && ls)", "echo $(pwd)/x", "x=(a b) && ls src/x"] {
            assert!(
                locations(command)
                    .iter()
                    .all(|location| !location.contains('(')),
                "{command}: {:?}",
                locations(command)
            );
        }
    }

    /// A saved-output rule needs its distinctive name spelled in literal text:
    /// `*.log` stands for `pi-bash-` with a wildcard and is an ordinary log
    /// glob, while `pi-*` and `tool-res*` spell enough of it to count.
    #[test]
    fn a_saved_output_glob_needs_its_distinctive_name_in_literal_text() {
        for path in [
            "/repo/log/*.log",
            "/tmp/*.log",
            "/tmp/p*.log",
            "/data/*/t*/tool_01",
            "/home/me/.c*/p*/x/s/t*/a.txt",
        ] {
            assert_eq!(output_spool(path, &mut 0), Ok(false), "{path}");
        }
        for path in [
            "/tmp/pi-*",
            "/tmp/*-bash-*.log",
            "/home/me/.claude/projects/x/s/tool-res*/a.txt",
            "/home/me/.c*/p*/x/s/*results/a.txt",
            "/data/*/tool-*/tool_01",
            "/tmp/c*/x/s/tasks/*",
        ] {
            assert_eq!(output_spool(path, &mut 0), Ok(true), "{path}");
        }
    }

    /// Codex trims each patch line before it reads a header, so indentation
    /// does not hide a target from the marker lookup; Hermes `search_files`
    /// names its file filter `file_glob`.
    #[test]
    fn indented_patch_headers_and_hermes_file_globs_reach_the_marker_lookup() {
        let patch = json!({
            "tool_name": "apply_patch",
            "tool_input": {"command": "*** Begin Patch\n*** Add File: x.rs\n+x\n  \t*** Update File: ../a/secrets.env\n+KEY\n*** End Patch"},
        });
        assert_eq!(
            capture_lookup_locations(AgentKind::Codex, &patch, "/work/b", None),
            Some(vec![
                "/work/b/x.rs".to_owned(),
                "/work/a/secrets.env".to_owned()
            ])
        );
        let search = json!({
            "tool_name": "search_files",
            "tool_input": {"pattern": "KEY", "path": "/work", "file_glob": "a/*.env"},
        });
        assert_eq!(
            capture_lookup_locations(AgentKind::Hermes, &search, "/work/b", None),
            Some(vec!["/work".to_owned(), "/work/a/*.env".to_owned()])
        );
    }

    /// A broken marker fails a search/list tool closed, as it does a file or
    /// shell tool: nothing proves its matches miss every ignored path.
    #[test]
    fn an_invalid_policy_makes_a_search_metadata_only() {
        let policy = CapturePolicy::resolve(CaptureSource::Invalid, "/repo", None);
        for raw in [
            json!({"tool_name": "Grep", "tool_input": {"pattern": "x"}}),
            json!({"tool_name": "Glob", "tool_input": {"pattern": "*.rs"}}),
        ] {
            assert_eq!(
                policy
                    .inspect(AgentKind::ClaudeCode, &raw, "/repo")
                    .protocol()
                    .disposition(),
                CaptureDisposition::MetadataOnly
            );
        }
        let inactive = CapturePolicy::resolve(CaptureSource::Absent, "/repo", None);
        assert_eq!(
            inactive
                .inspect(
                    AgentKind::ClaudeCode,
                    &json!({"tool_name": "Grep", "tool_input": {"pattern": "x"}}),
                    "/repo"
                )
                .protocol()
                .disposition(),
            CaptureDisposition::Keep
        );
    }

    /// Copilot CLI's runtime `view`/`create` names are file tools for that
    /// agent only; another agent's same-named tool stays unknown.
    #[test]
    fn copilot_cli_runtime_file_tools_are_scoped_to_copilot() {
        assert_eq!(
            agent_family(AgentKind::CopilotCli, "view"),
            ToolFamily::File
        );
        assert_eq!(
            agent_family(AgentKind::CopilotCli, "create"),
            ToolFamily::File
        );
        assert_eq!(
            agent_family(AgentKind::CopilotCli, "str_replace_editor"),
            ToolFamily::File
        );
        assert_eq!(
            agent_family(AgentKind::CopilotCli, "rg"),
            ToolFamily::SearchList
        );
        assert_eq!(
            agent_family(AgentKind::CopilotCli, "edit"),
            ToolFamily::File
        );
        assert_eq!(
            agent_family(AgentKind::ClaudeCode, "view"),
            ToolFamily::Unknown
        );
        assert_eq!(
            agent_family(AgentKind::OpenCode, "create"),
            ToolFamily::Unknown
        );
        let metadata = tool_observation_metadata(
            AgentKind::CopilotCli,
            &json!({"tool_name": "view", "tool_input": {"path": "src/lib.rs"}}),
            true,
        )
        .unwrap();
        assert_eq!(metadata.tool_family, ToolFamily::File);
    }

    #[test]
    fn unevaluated_shell_expansions_respect_quoting() {
        for (command, expected) in [
            ("cat docs/{adr,x}/1.md", true),
            ("cat docs/ad[r]/1.md", true),
            ("echo {1..3}", true),
            ("cat 'docs/{adr,x}'", false),
            ("cat \"docs/[ab]\"", false),
            ("cat docs/\\{a,b}", false),
            ("cat docs/\\[ab]", false),
            ("echo ${a,b} ${arr[0]}", false),
            ("find . -exec cat {} ;", false),
            ("[ -f x ]", false),
            ("[[ -d x ]]", false),
            ("git diff HEAD@{1}..HEAD", false),
            ("echo a, b} {c", false),
            ("cat [] x", false),
            // A leading `]` is a class member; the class still closes.
            ("cat docs/ad[]r]/1.md", true),
            ("cat docs/ad[!]x]/1.md", true),
            ("jq .[] x.json", false),
            // zsh glob groups and extglobs, but not subshells or substitutions.
            ("cat docs/(adr|x)/1.md", true),
            ("cat (docs|x)/adr/1.md", true),
            ("cat (docs)/adr/1.md", true),
            ("cat (docs/adr/1.md|x)", true),
            ("cat docs/@(adr)/1.md", true),
            ("(cd src && cargo test)", false),
            ("(cd src && cargo test) | tee log", false),
            ("echo $((1+2)) $(date) x=$(pwd)/y", false),
            ("diff <(ls src) <(ls tests)", false),
            ("arr=(a b)", false),
            ("cat '(adr|x)'", false),
            // ANSI-C strings: numeric and control escapes are not decoded.
            ("cat $'docs/\\x61dr/1.md'", true),
            ("cat $'docs/\\141dr/1.md'", true),
            ("cat $'docs/\\u0061dr'", true),
            ("cat $'it\\'s {a,b}'", false),
            ("cat $'a\\tb'", false),
        ] {
            assert_eq!(has_unevaluated_expansion(command), expected, "{command}");
        }
    }

    #[test]
    fn globstar_reaches_lets_double_star_span_any_depth() {
        assert!(globstar_reaches("/repo/**/*.md", "/repo/docs/adr", false));
        assert!(globstar_reaches("/**", "/repo/docs/adr", false));
        assert!(globstar_reaches("/repo/d*/**", "/repo/docs/adr", false));
        assert!(globstar_reaches(
            "/repo/docs/adr/sub/**",
            "/repo/docs/adr",
            false
        ));
        assert!(globstar_reaches("C:/**/x", "C:/Docs", true));
        assert!(!globstar_reaches("/repo/src/**", "/repo/docs/adr", false));
        assert!(!globstar_reaches("/other/**", "/repo/docs/adr", false));
        // Without `**` it is exactly `glob_reaches`.
        assert!(!globstar_reaches("/repo/*.md", "/repo/docs/adr", false));
        assert!(globstar_reaches("/repo/*/adr/x", "/repo/docs/adr", false));
        // `glob_reaches` alone does not recurse; matching uses `globstar_reaches`.
        assert!(!glob_reaches("/repo/**/*.md", "/repo/docs/adr", false));
    }

    /// Claude Code's Windows `PowerShell` tool was classified unknown, so its
    /// output was captured without `ignore_paths` ever seeing the command
    /// (#1186). Once it is a shell tool, a command naming an ignored path in
    /// Windows spelling, including PowerShell's `-Param:value` binding, must
    /// drop; one naming a public path must still be kept.
    #[test]
    fn claude_code_powershell_honors_ignored_paths_on_a_windows_host() {
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["docs/adr/**".into()],
            }),
            r"C:\repo",
            None,
        );
        let powershell = |command: &str| json!({"tool_name": "PowerShell", "tool_input": {"command": command}, "tool_use_id": "toolu_1"});
        for (command, expected) in [
            (r"Get-Content docs\adr\x.md", CaptureDisposition::Drop),
            (
                r"Get-Content -Path .\docs\adr\x.md",
                CaptureDisposition::Drop,
            ),
            (r"Get-Content -Path:docs\adr\x.md", CaptureDisposition::Drop),
            (r"type C:\repo\DOCS\ADR\x.md", CaptureDisposition::Drop),
            (r"Get-Content -Path:src\lib.rs", CaptureDisposition::Keep),
            ("git status", CaptureDisposition::Keep),
        ] {
            let decision = policy.inspect(AgentKind::ClaudeCode, &powershell(command), r"C:\repo");
            assert_eq!(
                decision.protocol().tool_family(),
                ToolFamily::NonFile,
                "{command}"
            );
            assert_eq!(decision.protocol().disposition(), expected, "{command}");
        }
        // A broken marker cannot prove a PowerShell command misses every
        // ignored path, exactly like Bash.
        let invalid = CapturePolicy::resolve(CaptureSource::Invalid, r"C:\repo", None);
        assert_eq!(
            invalid
                .inspect(AgentKind::ClaudeCode, &powershell("git status"), r"C:\repo")
                .protocol()
                .disposition(),
            CaptureDisposition::MetadataOnly,
        );
    }

    #[test]
    fn shell_matching_is_off_when_inactive_and_fails_closed_when_invalid_or_over_budget() {
        let raw = bash("cat docs/adr/x.md");
        let inactive = CapturePolicy::resolve(CaptureSource::Absent, "/repo", None);
        assert_eq!(
            inactive
                .inspect(AgentKind::ClaudeCode, &raw, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Keep
        );
        // A broken marker strips a shell call like a file call, but a
        // non-file tool with no command (`web_search`) keeps its body.
        let invalid = CapturePolicy::resolve(CaptureSource::Invalid, "/repo", None);
        let decision = invalid.inspect(AgentKind::ClaudeCode, &raw, "/repo");
        assert_eq!(
            decision.protocol().disposition(),
            CaptureDisposition::MetadataOnly
        );
        let body = metadata_only_body(Some("s"), Some("/repo"), &decision).to_string();
        assert!(!body.contains("docs/adr"), "{body}");
        assert_eq!(
            invalid
                .inspect(
                    AgentKind::ClaudeCode,
                    &json!({"tool_name": "web_search", "tool_input": {"query": "docs/adr"}}),
                    "/repo"
                )
                .protocol()
                .disposition(),
            CaptureDisposition::Keep
        );
        // Without a home directory `~/` is left literal and cannot match.
        let homeless = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["docs/adr/**".into()],
            }),
            "/repo",
            None,
        );
        assert_eq!(
            homeless
                .inspect(AgentKind::ClaudeCode, &bash("cat ~/notes/x.md"), "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Keep
        );
        let costly = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["docs/**/?".repeat(100)],
            }),
            "/repo",
            None,
        );
        let long = format!("cat {}", vec!["docs/x".repeat(400); 8].join(" "));
        assert_eq!(
            costly
                .inspect(AgentKind::ClaudeCode, &bash(&long), "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Drop
        );
    }

    #[test]
    fn shell_words_follow_posix_quoting_and_keep_windows_separators() {
        assert_eq!(
            shell_words(r#"a 'b c' "d \"e\"" f\ g h|i>j;k&&(l) C:\docs\adr\x.md"#),
            [
                "a",
                "b c",
                "d \"e\"",
                "f g",
                "h",
                "i",
                "j",
                "k",
                "l",
                r"C:\docs\adr\x.md"
            ]
        );
        assert_eq!(literal_prefix("/repo/docs/adr/**"), "/repo/docs/adr");
        assert_eq!(literal_prefix("/**"), "/");
        assert_eq!(literal_prefix("C:/*.md"), "C:/");
        assert_eq!(literal_prefix("/repo/secret.txt"), "/repo/secret.txt");
        assert!(glob_reaches("/repo/docs/*/x.md", "/repo/docs/adr", false));
        assert!(!glob_reaches("/repo/*.md", "/repo/docs/adr", false));
        assert!(glob_reaches("C:/Docs/*", "C:/docs/adr", true));
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
    #[test]
    fn invalid_marker_strips_shell_calls_with_unparseable_commands() {
        let policy = CapturePolicy::resolve(CaptureSource::Invalid, "/repo", None);
        for (agent, payload) in [
            (
                AgentKind::ClaudeCode,
                json!({"tool_name":"Bash","tool_input":{"command":7}}),
            ),
            (
                AgentKind::Codex,
                json!({"tool_name":"shell","tool_input":{"command":["cat", 7]}}),
            ),
            (
                AgentKind::ClaudeCode,
                json!({"tool_name":"Bash","tool_input":{"cmd_line":"cat secret/x.md"}}),
            ),
        ] {
            let decision = policy.inspect(agent, &payload, "/repo");
            assert_eq!(
                decision.protocol().disposition(),
                CaptureDisposition::MetadataOnly,
                "{payload}"
            );
        }
    }

    #[test]
    fn long_bash_lc_script_in_argv_is_not_dropped_by_the_match_budget() {
        let ignore_paths = (0..40).map(|i| format!("/repo/private{i}/**")).collect();
        let policy = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig { ignore_paths }),
            "/repo",
            None,
        );
        assert_eq!(policy.state, PolicyState::Active);
        let script = format!("{}ls a?.rs", "echo x; ".repeat(350));
        assert!(script.chars().count() < MAX_CANDIDATE_PATH_CHARS);
        // Control: as a string, the script splits into short words.
        let as_string = json!({"tool_name":"shell","tool_input":{"command": script}});
        assert_eq!(
            policy
                .inspect(AgentKind::Codex, &as_string, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Keep,
            "control"
        );
        let as_argv = json!({"tool_name":"shell","tool_input":{"command":["bash","-lc",script]}});
        assert_eq!(
            policy
                .inspect(AgentKind::Codex, &as_argv, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Keep,
            "innocuous script silently dropped by the match budget"
        );
    }
    #[test]
    fn shell_command_borrows_the_command_and_splits_only_on_request() {
        let line = json!({"command": "cat 'private notes/x.md' | head"});
        let Some(ShellCommand::Line(borrowed)) = shell_command(&line) else {
            panic!("a string command is kept as a line");
        };
        assert!(std::ptr::eq(borrowed, line["command"].as_str().unwrap()));
        // A quoted word holding several words is read like a command line.
        assert_eq!(
            ShellCommand::Line(borrowed).words(),
            ["cat", "private notes/x.md", "private", "notes/x.md", "head"]
        );

        let argv = json!({"cmd": ["cat", "private notes/x.md"]});
        assert_eq!(
            shell_command(&argv).unwrap().words(),
            ["cat", "private notes/x.md", "private", "notes/x.md"]
        );
        let script = format!("echo {}; ls", "x ".repeat(MAX_ARGV_PATH_CHARS));
        let argv = json!({"command": ["bash", "-lc", script]});
        let words = shell_command(&argv).unwrap().words();
        assert!(!words.contains(&script), "a long script is not one path");
        assert_eq!(words.last().map(String::as_str), Some("ls"));

        for args in [
            json!({"command": 7}),
            json!({"command": ["cat", 7]}),
            json!({"cmd_line": "cat x"}),
        ] {
            assert!(shell_command(&args).is_none(), "{args}");
        }
    }
    /// A string-form command used to keep any quoted script whole, so
    /// `bash -c 'cat <ignored>'` or `"$(cat <ignored>)"` was kept while the
    /// argv form dropped. Nested words are now read like a command line, to a
    /// bounded depth; deeper nesting fails closed.
    #[test]
    fn quoted_scripts_in_string_commands_are_read_like_command_lines() {
        let policy = shell_policy();
        let decide = |command: &str| {
            policy
                .inspect(AgentKind::ClaudeCode, &bash(command), "/repo")
                .protocol()
                .disposition()
        };
        for command in [
            "bash -c 'cat docs/adr/1.md'",
            "timeout 5 bash -c \"head docs/adr/1.md\"",
            "echo \"$(cat docs/adr/1.md)\"",
            "FOO=\"$(cat docs/adr/1.md)\"",
            "echo \"`cat docs/adr/1.md`\"",
            "bash -c \"sh -c 'cat docs/adr/1.md'\"",
            "python3 -c \"print(open('docs/adr/1.md').read())\"",
        ] {
            assert_eq!(decide(command), CaptureDisposition::Drop, "{command}");
        }
        for command in ["bash -c 'cargo test'", "echo \"$(git rev-parse HEAD)\""] {
            assert_eq!(decide(command), CaptureDisposition::Keep, "{command}");
        }
        // Five quoting levels: the innermost script is not read, so the
        // command proves nothing.
        let mut script = "cat docs/adr/1.md".to_owned();
        for _ in 0..MAX_SHELL_NESTING {
            script = format!("bash -c {}", shell_quote(&script));
        }
        assert_eq!(decide(&script), CaptureDisposition::Drop, "four levels");
        let deeper = format!(
            "bash -c {}",
            shell_quote(&script.replace("docs/adr", "src"))
        );
        assert_eq!(
            decide(&deeper),
            CaptureDisposition::MetadataOnly,
            "five levels"
        );
    }

    fn shell_quote(text: &str) -> String {
        format!("'{}'", text.replace('\'', "'\\''"))
    }

    /// Inline interpreter code can name a path no lexical match sees, so a
    /// command running it is metadata-only unless a literal names an ignored
    /// path outright. Running a script file or a module is unaffected.
    #[test]
    fn inline_interpreter_code_is_metadata_only_under_an_active_policy() {
        for (words, inline) in [
            (&["python3", "-c", "x"][..], true),
            (&["/usr/bin/python3.12", "-I", "-c", "x"], true),
            (&["python", "-Ic", "x"], true),
            (&["node", "-e", "x"], true),
            (&["node", "--eval=x"], true),
            (&["bun", "-p", "x"], true),
            (&["ruby", "-e", "x"], true),
            (&["perl", "-ne", "x"], true),
            (&["php", "-r", "x"], true),
            (&["deno", "eval", "x"], true),
            (&["python3", "-m", "pytest", "-c", "x"], false),
            (&["python3", "script.py"], false),
            (&["node", "scripts/build.js"], false),
            (&["ruby", "-Eutf-8", "x.rb"], false),
            (&["grep", "-e", "x"], false),
        ] {
            assert_eq!(runs_inline_code(words), inline, "{words:?}");
        }
        let policy = shell_policy();
        let argv =
            json!({"tool_name": "shell", "tool_input": {"command": ["python3", "-c", "print(1)"]}});
        assert_eq!(
            policy
                .inspect(AgentKind::Codex, &argv, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::MetadataOnly
        );
        let inactive = CapturePolicy::resolve(CaptureSource::Absent, "/repo", None);
        assert_eq!(
            inactive
                .inspect(AgentKind::Codex, &argv, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Keep
        );
    }

    /// `git show REV:path` names a file the way `cat path` does, resolved
    /// from the repository root; drive letters, URLs and host names are not
    /// object names.
    #[test]
    fn git_object_names_are_path_candidates() {
        assert_eq!(git_object_path("HEAD:docs/x.md"), Some("docs/x.md"));
        assert_eq!(git_object_path(":docs/x.md"), Some("docs/x.md"));
        assert_eq!(git_object_path(":2:docs/x.md"), Some("docs/x.md"));
        assert_eq!(git_object_path("HEAD@{1}:x"), Some("x"));
        assert_eq!(git_object_path(r"C:\docs\x.md"), None);
        assert_eq!(git_object_path("C:/docs/x.md"), None);
        assert_eq!(git_object_path("https://h/docs/x.md"), None);
        assert_eq!(git_object_path("NAME=a:b"), None);
        assert_eq!(git_object_path("--format=%h:%s"), None);
        let policy = shell_policy();
        for (command, cwd, expected) in [
            (
                "git show HEAD:docs/adr/1.md",
                "/repo",
                CaptureDisposition::Drop,
            ),
            (
                "git show HEAD:docs/adr/1.md",
                "/repo/src",
                CaptureDisposition::Drop,
            ),
            (
                "git show :./adr/1.md",
                "/repo/docs",
                CaptureDisposition::Drop,
            ),
            (
                "git show HEAD:src/main.rs",
                "/repo",
                CaptureDisposition::Keep,
            ),
        ] {
            assert_eq!(
                policy
                    .inspect(AgentKind::ClaudeCode, &bash(command), cwd)
                    .protocol()
                    .disposition(),
                expected,
                "{command} in {cwd}"
            );
        }
        // A Windows host keeps its drive paths out of object-name parsing.
        let windows = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["docs/adr/**".into()],
            }),
            r"C:\repo",
            None,
        );
        for (command, expected) in [
            (r"type C:\repo\src\x.md", CaptureDisposition::Keep),
            ("git show HEAD:docs/adr/1.md", CaptureDisposition::Drop),
        ] {
            assert_eq!(
                windows
                    .inspect(AgentKind::ClaudeCode, &bash(command), r"C:\repo")
                    .protocol()
                    .disposition(),
                expected,
                "{command}"
            );
        }
    }

    /// `$'…'` is decoded like the shell does, and `\o` outside quotes is
    /// `o`, so neither spelling hides an ignored path.
    #[test]
    fn ansi_c_strings_and_escaped_letters_are_read_like_the_shell() {
        assert_eq!(
            shell_words(r"cat $'docs/adr/1.md' $'a\tb' $'it\'s' $'\x61' $loc"),
            ["cat", "docs/adr/1.md", "a\tb", "it's", r"\x61", "$loc"]
        );
        assert_eq!(shell_words("echo `cat x`y"), ["echo", "cat", "x", "y"]);
        let policy = shell_policy();
        for (command, expected) in [
            ("cat $'docs/adr/1.md'", CaptureDisposition::Drop),
            ("cat $\"docs/adr/1.md\"", CaptureDisposition::Drop),
            (r"cat d\ocs/adr/1.md", CaptureDisposition::Drop),
            (r"cat docs/a\dr/1.\md", CaptureDisposition::Drop),
            (r"cat $'docs/\x61dr/1.md'", CaptureDisposition::MetadataOnly),
            ("cat $'src/main.rs'", CaptureDisposition::Keep),
            (r"cat s\rc/main.rs", CaptureDisposition::Keep),
        ] {
            assert_eq!(
                policy
                    .inspect(AgentKind::ClaudeCode, &bash(command), "/repo")
                    .protocol()
                    .disposition(),
                expected,
                "{command}"
            );
        }
    }

    /// Antigravity's `run_command` runs in its `Cwd` argument, so a relative
    /// argument resolves from there; another agent's `Cwd` key means nothing.
    #[test]
    fn antigravity_run_command_resolves_arguments_from_its_cwd() {
        let policy = shell_policy();
        let run = |command: &str, cwd: &str| json!({"toolCall": {"name": "run_command", "args": {"CommandLine": command, "Cwd": cwd}}});
        for (raw, expected) in [
            (run("cat 1.md", "/repo/docs/adr"), CaptureDisposition::Drop),
            (run("cat adr/1.md", "docs"), CaptureDisposition::Drop),
            (run("cat 1.md", "/repo/src"), CaptureDisposition::Keep),
            (run("cat docs/adr/1.md", "/repo"), CaptureDisposition::Drop),
        ] {
            assert_eq!(
                policy
                    .inspect(AgentKind::AntigravityCli, &raw, "/repo")
                    .protocol()
                    .disposition(),
                expected,
                "{raw}"
            );
        }
        let other = json!({"tool_name": "Bash", "tool_input": {"command": "cat 1.md", "Cwd": "/repo/docs/adr"}});
        assert_eq!(
            policy
                .inspect(AgentKind::ClaudeCode, &other, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Keep
        );
        assert_eq!(
            absolute_tool_location_paths(
                AgentKind::AntigravityCli,
                &run("ls", "/other/repo"),
                "/repo"
            ),
            Some(vec!["/other/repo".to_owned()])
        );
    }
    /// On a Windows host cmd's caret and PowerShell's backquote escape the
    /// next character whatever the shell tool, so their readings without the
    /// escape are matched; a POSIX shell reads both literally (control).
    #[test]
    fn windows_escape_characters_are_read_on_a_windows_host() {
        let windows = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["docs/adr/**".into()],
            }),
            r"C:\repo",
            None,
        );
        for (command, expected) in [
            (r"type docs\ad^r\x.md", CaptureDisposition::Drop),
            (r"Get-Content docs\ad`r\x.md", CaptureDisposition::Drop),
            (r"type C:\repo\DOCS\A^DR\x.md", CaptureDisposition::Drop),
            (r"type src\ma^in.rs", CaptureDisposition::Keep),
            ("echo a^b", CaptureDisposition::Keep),
        ] {
            let decision = windows.inspect(AgentKind::ClaudeCode, &bash(command), r"C:\repo");
            assert_eq!(decision.protocol().disposition(), expected, "{command}");
        }
        let posix = CapturePolicy::resolve(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["docs/adr/**".into()],
            }),
            "/repo",
            None,
        );
        for command in ["cat docs/ad^r/x.md", "cat docs/ad`r`/x.md"] {
            assert_eq!(
                posix
                    .inspect(AgentKind::ClaudeCode, &bash(command), "/repo")
                    .protocol()
                    .disposition(),
                CaptureDisposition::Keep,
                "{command}"
            );
        }
    }

    /// A macOS host reads `DOCS/ADR/x.md` as `docs/adr/x.md`, so file tools
    /// and shell arguments match without regard to case there, and only
    /// there (control: the same host-independent policy elsewhere).
    #[test]
    fn a_case_folding_host_matches_paths_without_regard_to_case() {
        let policy = |fold_case| {
            CapturePolicy::resolve_for_host(
                CaptureSource::Parsed(&CaptureConfig {
                    ignore_paths: vec!["docs/adr/**".into()],
                }),
                "/repo",
                None,
                fold_case,
            )
        };
        let read = json!({"tool_name": "Read", "tool_input": {"file_path": "DOCS/Adr/x.md"}});
        for (raw, label) in [(read, "read"), (bash("cat DOCS/Adr/x.md"), "bash")] {
            for (fold_case, expected) in [
                (true, CaptureDisposition::Drop),
                (false, CaptureDisposition::Keep),
            ] {
                assert_eq!(
                    policy(fold_case)
                        .inspect(AgentKind::ClaudeCode, &raw, "/repo")
                        .protocol()
                        .disposition(),
                    expected,
                    "{label} fold_case={fold_case}"
                );
            }
        }
        let public = json!({"tool_name": "Read", "tool_input": {"file_path": "SRC/x.md"}});
        assert_eq!(
            policy(true)
                .inspect(AgentKind::ClaudeCode, &public, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Keep
        );
        // Beyond ASCII: the full lowercase mapping, and a decomposed `É`
        // (`E` + U+0301) is the composed one.
        let unicode = CapturePolicy::resolve_for_host(
            CaptureSource::Parsed(&CaptureConfig {
                ignore_paths: vec!["Écrits/Ärger/**".into()],
            }),
            "/repo",
            None,
            true,
        );
        for path in [
            "écrits/ärger/x.md",
            "ÉCRITS/ÄRGER/x.md",
            "E\u{301}crits/A\u{308}rger/x.md",
        ] {
            let read = json!({"tool_name": "Read", "tool_input": {"file_path": path}});
            for raw in [read, bash(&format!("cat {path}"))] {
                assert_eq!(
                    unicode
                        .inspect(AgentKind::ClaudeCode, &raw, "/repo")
                        .protocol()
                        .disposition(),
                    CaptureDisposition::Drop,
                    "{path}"
                );
            }
        }
        let other = json!({"tool_name": "Read", "tool_input": {"file_path": "ecrits/arger/x.md"}});
        assert_eq!(
            unicode
                .inspect(AgentKind::ClaudeCode, &other, "/repo")
                .protocol()
                .disposition(),
            CaptureDisposition::Keep
        );
        assert!(
            !CapturePolicy::resolve(CaptureSource::Absent, "/repo", None).fold_case,
            "an inactive policy never folds"
        );
        assert_eq!(
            CapturePolicy::resolve(
                CaptureSource::Parsed(&CaptureConfig {
                    ignore_paths: vec!["x/**".into()],
                }),
                "/repo",
                None,
            )
            .fold_case,
            cfg!(target_os = "macos")
        );
    }

    /// Reading nested words and the new fail-closed syntax must not strip
    /// everyday commands that name no ignored path.
    #[test]
    fn common_commands_stay_kept_under_an_active_policy() {
        let policy = shell_policy();
        for command in [
            "cargo test",
            "cargo test -p ai-memory-hooks -- --nocapture",
            "cargo clippy --workspace --all-targets -- -D warnings",
            "git status",
            "git log --oneline -10",
            "git log --format='%h %s' -5",
            "git log --pretty=format:\"%h {a,b}\" -- src/",
            "git diff",
            "git diff --stat HEAD~1",
            "git diff HEAD~1 -- src/",
            "git show HEAD --stat",
            "git show HEAD:src/main.rs",
            "git commit -m \"fix: handle {a,b} and [x] in the parser\"",
            "ls",
            "ls -la src",
            "npm test",
            "npm run build -- --watch",
            "npx jest src/app.test.ts",
            "pytest -k \"not slow\" -x tests/",
            "python3 -m pytest -q",
            "python3 scripts/gen.py --out build/",
            "node scripts/build.js",
            "rg TODO src",
            "rg \"(foo|bar)baz\" src",
            "grep -rn \"fn main\\(\" src",
            "echo \"$(git rev-parse HEAD)\"",
            "(cd src && cargo test)",
            "docker compose up -d",
            "curl -s http://localhost:8080/health",
            // Near misses of the continuation, option-value, glued-flag,
            // URL, tilde and parameter-default readings.
            "cargo test \\\n  --workspace",
            "bash -o pipefail -c 'cargo test'",
            "set -o pipefail; cargo test",
            "python3 -W ignore -m pytest",
            "node -r ts-node/register scripts/build.ts",
            "grep -fpatterns.txt src",
            "cargo build -j4",
            "tar -czf out.tgz src",
            "gcc -Isrc -o build/app src/main.c",
            "make -C src",
            "curl -s file:///tmp/report.txt",
            "cat ~+/src/main.rs",
            "ls -la ~",
            "echo ${HOME:-/tmp} ${PWD}",
            "git log --oneline HEAD~3..HEAD",
            "git rebase -i HEAD~2",
            "echo a^b",
            // A quoted tilde is literal, and a query or fragment is not part
            // of a `file:` URL's path.
            "git commit -m \"fix ~approx\"",
            "grep -E '~[a-z]+' f",
            "echo 'a ~b c'",
            "sed -i 's/~old/~new/' src/main.rs",
            "curl -s 'file:///tmp/report.txt?x=1#top'",
            "echo ${A:-${B:-${C:-x}}}",
            "tail -f /tmp/claude-1000/-repo/0f1e/scratchpad/build.log",
        ] {
            assert_eq!(
                policy
                    .inspect(AgentKind::ClaudeCode, &bash(command), "/repo")
                    .protocol()
                    .disposition(),
                CaptureDisposition::Keep,
                "{command}"
            );
        }
    }
}
