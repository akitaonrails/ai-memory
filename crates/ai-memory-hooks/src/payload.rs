//! Wire envelope received on `POST /hook`.

use ai_memory_core::{
    AgentKind, OBSERVATION_BODY_MAX_BYTES, ObservationKind, Sanitizer, truncate_utf8_bytes,
};
use serde::{Deserialize, Serialize};

use crate::capture_policy::{
    ToolFamily, ToolObservationMetadata, tool_observation_metadata, tool_observation_outcome,
};

/// Durable excerpt ceiling for user prompts. Prompts retain more working
/// context than tool/notification summaries while remaining bounded.
pub const USER_PROMPT_EXCERPT_MAX_BYTES: usize = OBSERVATION_BODY_MAX_BYTES;

/// Durable excerpt ceiling for post-compaction summaries.
pub const POST_COMPACTION_EXCERPT_MAX_BYTES: usize = OBSERVATION_BODY_MAX_BYTES;

/// Durable excerpt ceiling for notifications.
pub const NOTIFICATION_EXCERPT_MAX_BYTES: usize = 2_000;

/// Durable excerpt ceiling for tool I/O summaries and short excerpts
/// (tool results, extension bodies, Stop assistant excerpts).
pub const TOOL_EXCERPT_MAX_BYTES: usize = 2_000;

/// Durable body ceiling the ingest funnel applies to an event's excerpt
/// **after** the sanitizer has scrubbed the full text.
///
/// The cap used to run inside excerpt extraction, before the sanitizer ever
/// saw the text, so a secret straddling the cutoff was cut in half and the
/// surviving prefix too short to match a pattern — stored unredacted (#1114),
/// the body-side twin of the #980 title-hint leak. Extraction now returns the
/// uncapped text and the router applies this cap to the scrubbed output,
/// mirroring `ai_memory_core::sanitize::Sanitized::new`.
#[must_use]
pub fn durable_body_cap(event: HookEvent) -> usize {
    match event {
        HookEvent::UserPrompt => USER_PROMPT_EXCERPT_MAX_BYTES,
        HookEvent::PostCompaction => POST_COMPACTION_EXCERPT_MAX_BYTES,
        HookEvent::Notification
        | HookEvent::PreToolUse
        | HookEvent::PostToolUse
        | HookEvent::Stop
        | HookEvent::Other => TOOL_EXCERPT_MAX_BYTES,
        _ => OBSERVATION_BODY_MAX_BYTES,
    }
}

/// Query-string parameters on `POST /hook`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookQuery {
    /// Lifecycle event identifier (kebab-case or snake_case).
    pub event: String,
    /// Agent CLI identifier (`claude-code`, `codex`, `cursor`, etc.).
    pub agent: Option<String>,
    /// Working directory of the agent at the time the hook fired.
    /// Most agents put this in the JSON body, but accepting it on the
    /// query string too lets `curl` / tests / non-Claude bridges
    /// populate it without constructing a body envelope.
    pub cwd: Option<String>,
    /// Workspace name override (typically declared by the agent's
    /// host-side hook via a `.ai-memory.toml` walk-up). When `None`
    /// the server falls back to `DEFAULT_WORKSPACE_NAME`.
    pub workspace: Option<String>,
    /// Project name override (same source as `workspace`). When
    /// `None` the server falls back to `basename(cwd)`.
    pub project: Option<String>,
    /// Session identifier supplied by a host-side bridge when the agent's
    /// native hook payload does not include one. Body values still win.
    pub session_id: Option<String>,
    /// Optional project derivation strategy from `.ai-memory.toml`.
    /// `repo-root` makes the server derive project identity from the
    /// main git repository root instead of `basename(cwd)`.
    pub project_strategy: Option<String>,
    /// Optional third-party extension namespace. When present, ai-memory
    /// preserves a validated source event name without expanding the
    /// closed core event vocabulary.
    pub extension: Option<String>,
    /// Optional explicit source event name for extension vocabularies.
    /// When omitted and `extension` is present, unknown `event` values
    /// are preserved as the source event.
    pub source_event: Option<String>,
    /// Per-project opt-in for `drop_subagent_captures`, forwarded by the
    /// host-side hook from a project's `.ai-memory.toml`. A truthy value
    /// (`1`/`true`/…) makes the server accept-but-drop this project's subagent
    /// captures; absent/falsy leaves them stored. Scoping the drop to the
    /// project that asked for it avoids a server-global switch that would shed
    /// subagent captures for every project on a shared instance.
    pub drop_subagent: Option<String>,
    /// Per-repo opt-in for `[recall] default_global`, forwarded by the
    /// host-side hook from a project's `.ai-memory.toml`. A truthy value makes
    /// the server publish `default_global` on this actor's `ActiveProject`, so
    /// a default-scoped `memory_query` / `memory_recent` searches every project
    /// instead of just the current one — the meta-repo case (e.g. `ai-memory`
    /// needing to see `ai-memory-ops` / `infra` without passing `global=true`).
    pub default_global: Option<String>,
    /// Invocation-scoped `ai-memory run` lease. Absent for every direct
    /// harness launch, preserving legacy capture and handoff behavior.
    pub managed_run: Option<String>,
    /// Client-side opt-in for assistant/Stop capture, baked onto the native
    /// `stop` hook command by `install-hooks --capture-assistant`. A truthy
    /// value tells the server the client deliberately attached a sanitized
    /// `_ai_memory_assistant` excerpt; the server still gates on its own
    /// `capture_assistant` config before persisting it (#196).
    pub capture_assistant: Option<String>,
    /// Client idempotency key, minted once when the event is spooled and
    /// re-sent verbatim on every retry of that entry. Lets the server drop
    /// a replay whose previous delivery succeeded but whose response was
    /// lost (the conservative-retry duplication vector). Absent on older
    /// clients; older servers ignore it — both directions keep today's
    /// behavior.
    pub ingest_key: Option<String>,
    /// Explicit root-only recovery propagation from `finalize-session --all-owners`.
    pub all_owners: Option<String>,
    /// Provenance of the `project` override: `marker` when a
    /// `.ai-memory.toml` named it, `repo-root` when the host hook derived it
    /// from the enclosing checkout. Absent on older clients (#394).
    pub project_src: Option<String>,
    /// Repository identity the client resolved for this checkout (#708):
    /// an explicit marker `identity`, or a normalised git remote. Paired
    /// with `identity_src`. Absent on older clients and for checkouts that
    /// declare a `project`, which keep routing by name.
    pub identity: Option<String>,
    /// Which rung produced `identity`: `explicit` or `git_remote`. Anything
    /// else, or a malformed identity, is ignored and the event routes by name.
    pub identity_src: Option<String>,
    /// An explicit marker `identity_style` (#1033). `path` names a
    /// remote-backed project from its repository path without the host;
    /// `host_path` preserves legacy naming. Omission uses the server default.
    pub identity_style: Option<String>,
    /// Compact JSON array of validated former project names from the local
    /// marker. Accepted only with a full git-remote identity.
    pub aliases: Option<String>,
}

/// Coalesced view of an incoming hook event after light parsing of the
/// body. We keep the original raw JSON around so consumers can extract
/// agent-specific fields they care about.
#[derive(Clone, Serialize)]
pub struct HookEnvelope {
    /// Mapped lifecycle event.
    pub event: HookEvent,
    /// Agent CLI identifier.
    pub agent: AgentKind,
    /// Session identifier from the body, or from the query string when a
    /// host-side bridge had to supply it. Required for everything except the
    /// initial `SessionStart`.
    pub session_id: Option<String>,
    /// Current working directory at the time of the event.
    pub cwd: Option<String>,
    /// Workspace name override declared by the hook (via marker file
    /// walk-up). Empty / `None` defers to `DEFAULT_WORKSPACE_NAME`.
    pub workspace_override: Option<String>,
    /// Project name override declared by the hook. Empty / `None`
    /// defers to `basename(cwd)`.
    pub project_override: Option<String>,
    /// Project derivation strategy declared by the hook marker.
    pub project_strategy: ProjectStrategy,
    /// Where `project_override` came from. Always [`ProjectSource::Unspecified`]
    /// when there is no override, so the two can never disagree (#394).
    pub project_source: ProjectSource,
    /// Repository identity from the client, already validated by
    /// [`ai_memory_core::repository_identity::accept_wire_identity`]. `None`
    /// routes by project name, as every event did before identities existed.
    pub identity: Option<ai_memory_core::repository_identity::RepositoryIdentity>,
    /// How a project this event creates is named; see
    /// [`ai_memory_core::repository_identity::IdentityStyle`].
    pub identity_style: ai_memory_core::repository_identity::IdentityStyle,
    /// Validated, normalized former project names supplied by the local marker.
    pub aliases: ai_memory_core::repository_identity::MarkerAliases,
    /// Whether an alias query was malformed or lacked a git-remote identity.
    pub aliases_invalid: bool,
    /// Whether this project opted into `drop_subagent_captures` via its
    /// `.ai-memory.toml` (forwarded as the `drop_subagent` query flag). The
    /// ingest router consults this per-event so the drop is scoped to the
    /// project that asked for it.
    pub drop_subagent_requested: bool,
    /// Whether this repo opted into `[recall] default_global` via its
    /// `.ai-memory.toml` (forwarded as the `default_global` query flag). The
    /// router publishes it on the actor's `ActiveProject` so default-scoped
    /// read tools broaden to a global search.
    pub recall_default_global_requested: bool,
    /// Explicit cross-owner recovery request (never honored for ordinary hooks).
    pub all_owners_requested: bool,
    /// Invocation-scoped managed-run id forwarded by the host hook.
    pub managed_run: Option<String>,
    /// Optional third-party extension namespace.
    pub extension: Option<String>,
    /// Optional source event name from the extension vocabulary.
    pub source_event: Option<String>,
    /// Whether the client requested assistant/Stop capture for this event
    /// (the `capture_assistant` query flag baked onto the native `stop`
    /// command). The server still gates on its own `capture_assistant` config
    /// before honoring it (#196).
    pub capture_assistant_requested: bool,
    /// Validated client idempotency key (`ingest_key` query param): 1–64
    /// ASCII `[A-Za-z0-9_-]` chars, else dropped at parse time. `Some` makes
    /// the ingest path dedup the event against a replayed delivery.
    pub ingest_key: Option<String>,
    /// Optional title hint extracted from the body.
    pub title_hint: Option<String>,
    /// Optional body excerpt extracted from the agent's raw payload.
    pub body_excerpt: Option<String>,
    /// Original event time (RFC 3339), when the client supplied one —
    /// notably backfill replaying a transcript's own per-event timestamps.
    /// Bounded and converted at the point of use via
    /// [`HookEnvelope::occurred_at_micros`]; never trust it unchecked.
    #[serde(default)]
    pub occurred_at: Option<String>,
    /// The agent's raw JSON, kept for forensics.
    pub raw: serde_json::Value,
}

/// Manual `Debug` that omits raw and derived hook content. A stray `?env` or
/// `%env` in a tracing span must not copy prompt or tool content into logs.
impl std::fmt::Debug for HookEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookEnvelope")
            .field("event", &self.event)
            .field("agent", &self.agent)
            .field("session_id", &self.session_id)
            .field("cwd", &self.cwd)
            .field("workspace_override", &self.workspace_override)
            .field("project_override", &self.project_override)
            .field("project_strategy", &self.project_strategy)
            .field("identity", &self.identity)
            .field("alias_count", &self.aliases.as_slice().len())
            .field("aliases_invalid", &self.aliases_invalid)
            .field("drop_subagent_requested", &self.drop_subagent_requested)
            .field(
                "recall_default_global_requested",
                &self.recall_default_global_requested,
            )
            .field("managed_run", &self.managed_run)
            .field(
                "capture_assistant_requested",
                &self.capture_assistant_requested,
            )
            .field("extension", &self.extension)
            .field("source_event", &self.source_event)
            // Client-controlled and only loosely shaped at this point (parsed
            // and bounded later, in `occurred_at_micros`), so it is capped
            // here rather than logged verbatim — a valid RFC 3339 timestamp
            // is well under this, only a hostile payload would hit it.
            .field(
                "occurred_at",
                &self
                    .occurred_at
                    .as_deref()
                    .map(|s| truncate_utf8_bytes(s, 64)),
            )
            .field(
                "title_hint",
                &self.title_hint.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "body_excerpt",
                &self.body_excerpt.as_ref().map(|_| "<redacted>"),
            )
            .field("raw", &"<redacted>")
            .finish()
    }
}

/// Keys by which agent harnesses tag a hook event as belonging to a SUBAGENT
/// (a nested/spawned agent session) rather than the top-level session. Grok
/// sets `subagentType` (on its tool-use events); Claude Code sets `agent_id`
/// on subagent events. `agent_type` alone is not a subagent marker: Claude
/// Code also sets it on top-level sessions launched with `--agent`. The set
/// is a union so one check covers every harness that signals subagent-ness;
/// a harness that does not signal it simply never matches.
const SUBAGENT_MARKER_KEYS: &[&str] = &["subagentType", "agent_id"];

/// True when the raw hook payload carries a non-empty subagent marker — i.e.
/// the event originates from a spawned subagent session. The ingest router
/// consults this to optionally drop subagent captures (the
/// `drop_subagent_captures` setting). Only top-level string keys are inspected.
pub(crate) fn body_is_subagent(raw: &serde_json::Value) -> bool {
    SUBAGENT_MARKER_KEYS.iter().any(|key| {
        raw.get(*key)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

/// Truthy interpretation of a query-string flag (`1`/`true`/`yes`/`on`,
/// case-insensitive). Used for the per-project `drop_subagent` opt-in the
/// host-side hook forwards from a project's `.ai-memory.toml`.
pub(crate) fn query_flag_truthy(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Whether a forwarded flag was explicitly turned off (`0` / `false` / `no` /
/// `off`, any case). An absent flag is not falsy: settings that default on,
/// like `[profile] contribute`, stay on unless a marker says otherwise.
pub(crate) fn query_flag_falsy(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "no" | "off")
    )
}

pub(crate) fn marker_aliases_from_wire(
    project: Option<&str>,
    project_source: ProjectSource,
    identity: Option<&ai_memory_core::repository_identity::RepositoryIdentity>,
    raw: Option<&str>,
) -> Result<
    ai_memory_core::repository_identity::MarkerAliases,
    ai_memory_core::repository_identity::MarkerAliasError,
> {
    let aliases = ai_memory_core::repository_identity::accept_wire_aliases(raw)?;
    if raw.is_some()
        && (project.is_none_or(|project| project.trim().is_empty())
            || project_source != ProjectSource::Marker
            || !identity.is_some_and(|identity| {
                identity.source == ai_memory_core::repository_identity::IdentitySource::GitRemote
            }))
    {
        return Err(ai_memory_core::repository_identity::MarkerAliasError::InvalidWire);
    }
    Ok(aliases)
}

/// How the hook router derives a project name when no explicit
/// `project` override is present.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectStrategy {
    /// Preserve v1 behavior: `project = basename(cwd)`.
    #[default]
    Basename,
    /// Opt-in marker behavior: `project = basename(main git repo root)`.
    RepoRoot,
}

impl ProjectStrategy {
    /// Parse a query-string marker value. Unknown values are ignored so a
    /// typo cannot route sessions into surprising new buckets.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("repo-root" | "repo_root") => Self::RepoRoot,
            _ => Self::Basename,
        }
    }

    /// Stable cache-key representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Basename => "basename",
            Self::RepoRoot => "repo-root",
        }
    }
}

/// Where a `project` override on the wire came from.
///
/// The host-side hook sends `project=` for two very different reasons: a
/// `.ai-memory.toml` marker naming the project outright, and `repo-root`
/// derivation of the enclosing checkout's name (which must run host-side —
/// a containerized server cannot see the client's checkout). Both arrive as
/// the same query parameter, so before #394's `sticky` knob the router could
/// not honor one while overriding the other. This discriminator is that
/// missing signal: a marker is a deliberate rescope and always wins, while a
/// repo-root derivation is just "where the cwd happens to be" and may yield
/// to the session under `[routing] mid_session = "sticky"`.
///
/// Older clients omit the parameter entirely and parse as [`Self::Unspecified`],
/// which keeps their overrides authoritative — the pre-#394 behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectSource {
    /// No provenance declared: either no override at all, or a client too
    /// old to tag one. Treated as authoritative, never as fell-through.
    #[default]
    Unspecified,
    /// A `.ai-memory.toml` marker named this project explicitly.
    Marker,
    /// The host hook derived it from the enclosing git repo root under the
    /// `repo-root` strategy.
    RepoRoot,
}

impl ProjectSource {
    /// Parse the `project_src` query value. Unknown values fall back to
    /// [`Self::Unspecified`] so a typo can never downgrade an override into
    /// something the session is allowed to overrule.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("marker") => Self::Marker,
            Some("repo-root" | "repo_root") => Self::RepoRoot,
            _ => Self::Unspecified,
        }
    }

    /// Stable wire/log representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Marker => "marker",
            Self::RepoRoot => "repo-root",
        }
    }

    /// Whether an override from this source may yield to session-sticky
    /// attribution. Only host-side repo-root derivation may: it carries no
    /// operator intent, just the cwd's enclosing checkout.
    #[must_use]
    pub const fn yields_to_session(self) -> bool {
        matches!(self, Self::RepoRoot)
    }
}

/// Discriminator for the lifecycle event that triggered the hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookEvent {
    /// New session started (capture cwd + model).
    SessionStart,
    /// User submitted a prompt.
    UserPrompt,
    /// Agent is about to call a tool.
    PreToolUse,
    /// Agent finished a tool call.
    PostToolUse,
    /// Compaction event (context window pressure).
    PreCompact,
    /// Post-compaction event (Devin-specific, after context compaction).
    PostCompaction,
    /// Agent emitted a notification.
    Notification,
    /// Agent finished its turn (interactive `/stop` or natural end).
    Stop,
    /// Session ended (final).
    SessionEnd,
    /// A subagent (nested/spawned child session) started.
    SubagentStart,
    /// A subagent finished.
    SubagentStop,
    /// Anything else.
    Other,
}

impl HookEvent {
    /// Parse a kebab- or snake-case event identifier into [`HookEvent`].
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "session-start" | "session_start" | "SessionStart" | "sessionStart" => {
                Self::SessionStart
            }
            "user-prompt" | "user_prompt" | "user-prompt-submit" | "user_prompt_submit"
            | "UserPromptSubmit" | "beforeSubmitPrompt" => Self::UserPrompt,
            "pre-tool-use" | "pre_tool_use" | "PreToolUse" | "preToolUse" | "BeforeTool" => {
                Self::PreToolUse
            }
            "post-tool-use" | "post_tool_use" | "PostToolUse" | "postToolUse"
            | "postToolUseFailure" | "PostToolUseFailure" | "AfterTool" => Self::PostToolUse,
            "pre-compact" | "pre_compact" | "PreCompact" | "preCompact" | "PreCompress" => {
                Self::PreCompact
            }
            "post-compaction" | "post_compaction" | "PostCompaction" => Self::PostCompaction,
            "notification" | "Notification" => Self::Notification,
            "stop" | "Stop" => Self::Stop,
            "session-end" | "session_end" | "SessionEnd" | "sessionEnd" => Self::SessionEnd,
            "subagent-start" | "subagent_start" | "SubagentStart" | "subagentStart" => {
                Self::SubagentStart
            }
            "subagent-stop" | "subagent_stop" | "SubagentStop" | "subagentStop"
            | "subagent-end" | "SubagentEnd" => Self::SubagentStop,
            _ => Self::Other,
        }
    }

    /// Map to the storage-level [`ObservationKind`].
    #[must_use]
    pub const fn to_observation_kind(self) -> ObservationKind {
        match self {
            Self::SessionStart => ObservationKind::SessionStart,
            Self::UserPrompt => ObservationKind::UserPrompt,
            Self::PreToolUse => ObservationKind::PreToolUse,
            Self::PostToolUse => ObservationKind::PostToolUse,
            Self::PreCompact => ObservationKind::PreCompact,
            Self::PostCompaction => ObservationKind::PostCompaction,
            Self::Notification => ObservationKind::Notification,
            Self::Stop => ObservationKind::Stop,
            Self::SessionEnd => ObservationKind::SessionEnd,
            // Subagent lifecycle events are normally dropped (drop_subagent_captures);
            // bucket as Other for the flag-off path rather than growing ObservationKind.
            Self::SubagentStart | Self::SubagentStop => ObservationKind::Other,
            Self::Other => ObservationKind::Other,
        }
    }
}

/// Parse an agent identifier into [`AgentKind`]. Unknown values map to
/// [`AgentKind::Other`].
#[must_use]
pub fn parse_agent(s: &str) -> AgentKind {
    AgentKind::from_wire(s)
}

/// Identify the harness from the payload itself when it carries an
/// unambiguous vendor marker, overriding the `?agent=` the hook command
/// declared.
///
/// The Cursor CLI also loads and runs the hook commands declared in Claude
/// Code's `~/.claude/settings.json` (its Claude Code config compatibility
/// path, alongside `~/.cursor/hooks.json`). Those commands were installed by
/// `install-hooks --agent claude-code`, so they hardcode
/// `--agent claude-code` — and a Cursor-driven session was therefore stored
/// with `agent_kind = claude-code`. The query string is the *installer's*
/// guess; `cursor_version` is stamped on every Cursor hook payload and never
/// appears in a Claude Code one, so the body is the stronger evidence.
///
/// Returns `None` when the payload carries no vendor marker, leaving the
/// declared `?agent=` untouched.
#[must_use]
pub fn agent_from_payload(raw: &serde_json::Value) -> Option<AgentKind> {
    extract_string(raw, &["cursor_version"]).map(|_| AgentKind::Cursor)
}

/// Session and checkout fields extracted from the canonical hook payload shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookPayloadContext {
    /// Harness-native session identity.
    pub session_id: Option<String>,
    /// Current checkout directory.
    pub cwd: Option<String>,
}

/// Extract the session and checkout exactly as [`HookEnvelope`] does.
#[must_use]
pub fn hook_payload_context(raw: &serde_json::Value) -> HookPayloadContext {
    let session_id = extract_string(
        raw,
        &[
            "session_id",
            "sessionId",
            "sessionID",
            "session",
            "conversationId",
        ],
    )
    .or_else(|| {
        extract_string_path(
            raw,
            &[
                &["info", "id"],
                &["properties", "sessionID"],
                &["properties", "info", "id"],
                &["event", "properties", "sessionID"],
                &["event", "properties", "info", "id"],
                &["payload", "info", "id"],
                &["payload", "properties", "sessionID"],
                &["payload", "properties", "info", "id"],
            ],
        )
    });
    let cwd = extract_string(raw, &["cwd", "current_dir", "working_dir", "directory"])
        .or_else(|| extract_first_string_array_item(raw, &["workspacePaths", "workspace_roots"]))
        .or_else(|| {
            extract_string_path(
                raw,
                &[
                    &["path", "cwd"],
                    &["info", "directory"],
                    &["properties", "info", "directory"],
                    &["event", "properties", "info", "directory"],
                    &["payload", "path", "cwd"],
                    &["payload", "info", "directory"],
                    &["payload", "properties", "info", "directory"],
                ],
            )
        });
    HookPayloadContext { session_id, cwd }
}

impl HookEnvelope {
    /// Build an envelope from the parsed query + the body JSON. Performs
    /// best-effort extraction of `session_id` / `cwd` / a body excerpt
    /// from common shapes used by Claude Code, Codex, and OpenCode hook
    /// payloads.
    #[must_use]
    pub fn from_query_and_body(query: HookQuery, mut raw: serde_json::Value) -> Self {
        let event = HookEvent::parse(&query.event);
        let agent = agent_from_payload(&raw)
            .unwrap_or_else(|| query.agent.as_deref().map_or(AgentKind::Other, parse_agent));
        let context = hook_payload_context(&raw);
        let session_id = context
            .session_id
            .or_else(|| query.session_id.filter(|s| !s.is_empty()));
        // Body cwd wins over the query-string fallback: the body is
        // what agent CLIs natively send, so any query-string `cwd` is
        // a bridge / test override that should defer to live data.
        let cwd = context.cwd.or_else(|| query.cwd.filter(|s| !s.is_empty()));
        let workspace_override = query.workspace.filter(|s| !s.is_empty());
        let project_override = query.project.filter(|s| !s.is_empty());
        let project_strategy = ProjectStrategy::parse(query.project_strategy.as_deref());
        // Provenance describes an override, so it is meaningless without one.
        // Pinning it to `Unspecified` here keeps every downstream check from
        // having to re-test `project_override.is_some()` alongside it.
        let project_source = if project_override.is_some() {
            ProjectSource::parse(query.project_src.as_deref())
        } else {
            ProjectSource::Unspecified
        };
        let identity = match (query.identity.as_deref(), query.identity_src.as_deref()) {
            (Some(identity), Some(source)) => {
                ai_memory_core::repository_identity::accept_wire_identity(identity, source)
            }
            _ => None,
        };
        let identity_style = query
            .identity_style
            .as_deref()
            .and_then(ai_memory_core::repository_identity::IdentityStyle::from_str_opt)
            .unwrap_or_default();
        let aliases = marker_aliases_from_wire(
            project_override.as_deref(),
            project_source,
            identity.as_ref(),
            query.aliases.as_deref(),
        );
        let aliases_invalid = aliases.is_err();
        let aliases = if aliases_invalid {
            ai_memory_core::repository_identity::MarkerAliases::default()
        } else {
            aliases.unwrap_or_default()
        };
        let drop_subagent_requested = query_flag_truthy(query.drop_subagent.as_deref());
        let recall_default_global_requested = query_flag_truthy(query.default_global.as_deref());
        let all_owners_requested = query_flag_truthy(query.all_owners.as_deref());
        let managed_run = query.managed_run.filter(|value| !value.trim().is_empty());
        let capture_assistant_requested = query_flag_truthy(query.capture_assistant.as_deref());
        let extension = normalize_extension_name(query.extension.as_deref());
        let source_event = extension.as_ref().and_then(|_| {
            let raw_source = query
                .source_event
                .as_deref()
                .or_else(|| (event == HookEvent::Other).then_some(query.event.as_str()))?;
            normalize_source_event(raw_source)
        });
        let extension = if source_event.is_some() {
            extension
        } else {
            None
        };
        if agent == AgentKind::AntigravityCli {
            crate::antigravity::enrich_antigravity_step_output(&mut raw, event, None);
        }
        let tool_metadata = tool_observation_metadata(agent, &raw, event == HookEvent::PreToolUse);
        let closed_tool_event = matches!(event, HookEvent::PreToolUse | HookEvent::PostToolUse)
            && closed_tool_agent(agent);
        let title_hint = if closed_tool_event {
            tool_metadata.as_ref().map(safe_tool_title).or_else(|| {
                (event == HookEvent::PostToolUse && agent == AgentKind::OpenCode)
                    .then(|| legacy_tool_title(event, agent, &raw))
                    .flatten()
            })
        } else if matches!(event, HookEvent::PreToolUse | HookEvent::PostToolUse) {
            legacy_tool_title(event, agent, &raw)
        } else {
            best_title_hint(event, &raw).or_else(|| {
                source_event
                    .as_deref()
                    .map(|source| extension_title_hint(&raw, source))
            })
        };
        let body_excerpt = if closed_tool_event {
            safe_tool_body(event, tool_metadata.as_ref(), agent, &raw).or_else(|| {
                (event == HookEvent::PostToolUse && agent == AgentKind::OpenCode)
                    .then(|| legacy_tool_body(event, agent, &raw))
                    .flatten()
            })
        } else if matches!(event, HookEvent::PreToolUse | HookEvent::PostToolUse) {
            legacy_tool_body(event, agent, &raw)
        } else {
            best_body_excerpt(event, &raw).or_else(|| {
                source_event
                    .as_deref()
                    .and_then(|_| extension_body_excerpt(&raw))
            })
        };
        let ingest_key = query.ingest_key.filter(|k| valid_ingest_key(k));
        // Original event time (RFC 3339), when the client carried one — currently
        // only `ai-memory backfill`, replaying a transcript's own per-event
        // timestamps. This is numeric metadata, not text, so it never goes
        // through the sanitizer (invariant #6 is about untrusted TEXT reaching
        // storage/prompts unscrubbed; a timestamp has no such surface). It is
        // still client-controlled input over the hook endpoint, so it is bounded
        // at parse time in `occurred_at_micros`, not trusted here.
        //
        // Read from the top-level body only, unlike `extract_string`'s nested
        // `payload`/`event`/`properties`/`info`/`path` search: that search
        // exists for text fields real agent harnesses nest under those keys,
        // but a real harness payload happening to carry an `occurred_at` key
        // somewhere in that structure must not be mistaken for backfill's own
        // top-level field.
        let occurred_at = raw
            .get("occurred_at")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Self {
            event,
            agent,
            session_id,
            cwd,
            workspace_override,
            project_override,
            project_strategy,
            project_source,
            identity,
            identity_style,
            aliases,
            aliases_invalid,
            drop_subagent_requested,
            recall_default_global_requested,
            all_owners_requested,
            managed_run,
            capture_assistant_requested,
            ingest_key,
            extension,
            source_event,
            title_hint,
            body_excerpt,
            occurred_at,
            raw,
        }
    }

    /// Parse and bound [`Self::occurred_at`] into a microsecond timestamp
    /// suitable for `NewSession`/`NewObservation::occurred_at`.
    ///
    /// `occurred_at` arrives from a client over the hook endpoint, so it is
    /// bounded here rather than trusted outright: only strictly positive
    /// values that are no more than [`MAX_OCCURRED_AT_FUTURE_SKEW_MICROS`]
    /// ahead of server time are honored. Anything else — an unparseable
    /// string, a non-positive value, or a suspicious far-future timestamp —
    /// resolves to `None`, which callers fall back to "now" for. Hooks are
    /// fire-and-forget, so this never errors.
    #[must_use]
    pub fn occurred_at_micros(&self) -> Option<i64> {
        let parsed = self
            .occurred_at
            .as_deref()?
            .parse::<jiff::Timestamp>()
            .ok()?
            .as_microsecond();
        let now = jiff::Timestamp::now().as_microsecond();
        if parsed > 0 && parsed <= now.saturating_add(MAX_OCCURRED_AT_FUTURE_SKEW_MICROS) {
            Some(parsed)
        } else {
            None
        }
    }
}

/// How far ahead of server time a client-supplied `occurred_at` may sit
/// before it is treated as bogus (clock skew, or a hostile client) and
/// dropped to `None` instead of being trusted.
const MAX_OCCURRED_AT_FUTURE_SKEW_MICROS: i64 = 5 * 60 * 1_000_000;

/// An ingest key is client-controlled input: accept only short, plain tokens
/// (1–64 ASCII alphanumerics, `-` or `_` — a UUID simple/hyphenated form
/// fits). Anything else is treated as absent rather than rejected, so a
/// malformed key degrades to today's at-least-once behavior instead of a 4xx.
fn valid_ingest_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn legacy_tool_title(
    event: HookEvent,
    agent: AgentKind,
    raw: &serde_json::Value,
) -> Option<String> {
    let payload = (event == HookEvent::PostToolUse && agent == AgentKind::OpenCode)
        .then(|| raw.get("payload"))
        .flatten()?;
    extract_string(payload, &["tool"]).filter(|_| {
        extract_content(
            payload,
            &["tool_response", "tool_output", "output", "result", "error"],
        )
        .is_some()
    })
}

fn legacy_tool_body(event: HookEvent, agent: AgentKind, raw: &serde_json::Value) -> Option<String> {
    let payload = (event == HookEvent::PostToolUse && agent == AgentKind::OpenCode)
        .then(|| raw.get("payload"))
        .flatten()?;
    let tool = extract_string(payload, &["tool"])?;
    let result = extract_content(
        payload,
        &["tool_response", "tool_output", "output", "result"],
    )
    .or_else(|| extract_content(payload, &["error"]))?;
    Some(format!("tool: {tool}\n---\n{result}"))
}

const fn closed_tool_agent(agent: AgentKind) -> bool {
    matches!(
        agent,
        AgentKind::ClaudeCode
            | AgentKind::CommandCode
            | AgentKind::Codex
            // Cursor's own hooks and its copies of the Claude Code hooks carry
            // Claude's snake_case tool aliases.
            | AgentKind::Cursor
            | AgentKind::OpenCode
            | AgentKind::Pi
            | AgentKind::Omp
            | AgentKind::AntigravityCli
            | AgentKind::Grok
            | AgentKind::Hermes
            | AgentKind::Pool
            | AgentKind::Zcode
            | AgentKind::CopilotCli
            | AgentKind::Grizzybot
    )
}

fn safe_tool_title(metadata: &ToolObservationMetadata) -> String {
    format!(
        "tool {}",
        serde_json::to_string(&metadata.tool_family)
            .unwrap_or_default()
            .trim_matches('"')
    )
}

/// `true` when `title` is one this module produced from a [`ToolFamily`]
/// rather than from the harness's own tool name.
///
/// Derived from the same serde representation [`safe_tool_title`] writes, so
/// a new `ToolFamily` variant is recognised here without a second edit — the
/// two cannot drift into disagreeing about what this module emits.
///
/// The distinction matters downstream: a family is a partition of the calls,
/// not a name for what they did, so a reader gains nothing from being told a
/// session spent itself "across tool non-file and tool unknown".
pub(crate) fn is_safe_tool_title(title: &str) -> bool {
    title.strip_prefix("tool ").is_some_and(|family| {
        serde_json::from_value::<ToolFamily>(serde_json::Value::String(family.to_owned())).is_ok()
    })
}

/// Map a stored observation title back to the [`ToolFamily`] it was derived
/// from, recognising **both** spellings this crate emits for a family label:
/// the bare `canonical_tool_name` form the reserved-protocol path writes
/// ("file" / "search-list" / "non-file" / "unknown") and the `"tool "`-prefixed
/// [`safe_tool_title`] form every closed-tool agent writes ("tool file", …).
///
/// Returns `None` for a harness's own raw tool name ("edit", "bash", "Read"),
/// which is never one of these labels. That distinction is load-bearing: it is
/// what keeps the family labels out of handoffs, titles, and the file-activity
/// heuristic while a real tool name still flows through. Shares the serde
/// representation with [`safe_tool_title`] and [`is_safe_tool_title`], so a new
/// `ToolFamily` variant is recognised here without a second edit.
pub(crate) fn tool_family_from_title(title: &str) -> Option<ToolFamily> {
    let family = title.strip_prefix("tool ").unwrap_or(title);
    serde_json::from_value::<ToolFamily>(serde_json::Value::String(family.to_owned())).ok()
}

fn safe_tool_body(
    event: HookEvent,
    metadata: Option<&ToolObservationMetadata>,
    agent: AgentKind,
    raw: &serde_json::Value,
) -> Option<String> {
    let metadata = metadata?;
    let mut summary = format!(
        "tool_family: {}",
        serde_json::to_string(&metadata.tool_family)
            .ok()?
            .trim_matches('"')
    );
    if let Some(id) = &metadata.tool_call_id {
        summary.push_str("\ntool_call_id: ");
        summary.push_str(id);
    }
    match event {
        HookEvent::PreToolUse => Some(summary),
        HookEvent::PostToolUse => {
            summary.push_str("\noutcome: ");
            summary.push_str(tool_observation_outcome(agent, raw).as_str());
            if metadata.tool_family == crate::capture_policy::ToolFamily::Unknown {
                return Some(summary);
            }
            let result = if agent == AgentKind::Codex {
                // Codex's native schema has one top-level JSON response.
                // Do not promote unrelated aliases or nested payloads to output.
                raw.get("tool_response").and_then(value_to_text)
            } else if agent == AgentKind::CopilotCli {
                // Copilot CLI's VS-Code-compatible `PostToolUse` nests the
                // model-facing text at `tool_result.text_result_for_llm`
                // (#1040); read only that documented field so `result_type`
                // never leaks in through an object-stringify fallback.
                raw.pointer("/tool_result/text_result_for_llm")
                    .and_then(value_to_text)
                    .or_else(|| extract_content(raw, &["error"]))
            } else if agent == AgentKind::Hermes {
                // Hermes 3.x nests the tool result inside `extra.result`
                // (`agent/shell_hooks.py::_payload_fields` keeps everything
                // not in the top-level key set inside `extra`). Read only
                // that documented field plus `error_message` so unrelated
                // `extra` telemetry (task_id, middleware_trace, …) never
                // leaks in through an object-stringify fallback.
                raw.pointer("/extra/result")
                    .and_then(value_to_text)
                    .or_else(|| raw.pointer("/extra/error_message").and_then(value_to_text))
                    .or_else(|| extract_content(raw, &["error"]))
            } else {
                extract_content(raw, &["tool_response", "tool_output", "output", "result"])
                    .or_else(|| extract_content(raw, &["error"]))
                    .or_else(|| antigravity_edit_content(agent, raw))
            }
            .unwrap_or_else(|| "(no output captured)".into());
            summary.push_str("\n---\n");
            summary.push_str(&result);
            Some(summary)
        }
        _ => None,
    }
}

fn antigravity_edit_content(agent: AgentKind, raw: &serde_json::Value) -> Option<String> {
    if agent != AgentKind::AntigravityCli {
        return None;
    }
    let tool_call = raw.get("toolCall")?;
    let tool = tool_call.get("name")?.as_str()?;
    if !matches!(
        tool.to_ascii_lowercase().as_str(),
        "write_to_file" | "replace_file_content" | "multi_replace_file_content"
    ) {
        return None;
    }
    let args = tool_call.get("args")?;
    extract_content(
        args,
        &["ReplacementContent", "CodeContent", "ReplacementChunks"],
    )
}

fn extract_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        for candidate in extraction_candidates(value) {
            if let Some(s) = candidate.get(*key).and_then(serde_json::Value::as_str)
                && !s.is_empty()
            {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn extract_string_path(value: &serde_json::Value, paths: &[&[&str]]) -> Option<String> {
    for path in paths {
        if let Some(s) = value_at_path(value, path).and_then(serde_json::Value::as_str)
            && !s.is_empty()
        {
            return Some(s.to_string());
        }
    }
    None
}

fn extract_scalar_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        for candidate in extraction_candidates(value) {
            if let Some(value) = candidate.get(*key) {
                if let Some(s) = value.as_str()
                    && !s.is_empty()
                {
                    return Some(s.to_string());
                }
                if let Some(n) = value.as_i64() {
                    return Some(n.to_string());
                }
                if let Some(n) = value.as_u64() {
                    return Some(n.to_string());
                }
            }
        }
    }
    None
}

fn extract_first_string_array_item(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        for candidate in extraction_candidates(value) {
            if let Some(items) = candidate.get(*key).and_then(serde_json::Value::as_array) {
                for item in items {
                    if let Some(s) = item.as_str()
                        && !s.is_empty()
                    {
                        return Some(s.to_string());
                    }
                }
            }
        }
    }
    None
}

fn value_at_path<'a>(
    mut value: &'a serde_json::Value,
    path: &[&str],
) -> Option<&'a serde_json::Value> {
    for segment in path {
        value = value.get(*segment)?;
    }
    Some(value)
}

fn extraction_candidates(value: &serde_json::Value) -> Vec<&serde_json::Value> {
    let mut out = Vec::new();
    push_candidates(&mut out, value);
    if let Some(payload) = value.get("payload") {
        push_candidates(&mut out, payload);
    }
    if let Some(event) = value.get("event") {
        push_candidates(&mut out, event);
    }
    out
}

fn push_candidates<'a>(out: &mut Vec<&'a serde_json::Value>, value: &'a serde_json::Value) {
    out.push(value);
    if let Some(properties) = value.get("properties") {
        out.push(properties);
        if let Some(info) = properties.get("info") {
            out.push(info);
        }
    }
    if let Some(info) = value.get("info") {
        out.push(info);
    }
    if let Some(path) = value.get("path") {
        out.push(path);
    }
}

fn best_title_hint(event: HookEvent, raw: &serde_json::Value) -> Option<String> {
    match event {
        HookEvent::SessionStart => extract_string(raw, &["model", "title"]).and_then(first_line),
        HookEvent::UserPrompt => {
            // Kimi Code sends `prompt` as content blocks
            // (`[{"type":"text","text":...}]`); `extract_content` flattens
            // them and returns identical values for plain-string agents.
            extract_content(raw, &["prompt", "message", "text"]).and_then(first_line)
        }
        HookEvent::PreToolUse | HookEvent::PostToolUse => {
            extract_string(raw, &["tool", "tool_name", "name"])
                .or_else(|| extract_string_path(raw, &[&["toolCall", "name"]]))
                .or_else(|| {
                    extract_scalar_string(raw, &["stepIdx"]).map(|step| format!("step {step}"))
                })
        }
        HookEvent::Notification => extract_string(raw, &["message", "text"]).and_then(first_line),
        HookEvent::PostCompaction => extract_string(raw, &["summary"]).and_then(first_line),
        _ => None,
    }
}

fn extension_title_hint(raw: &serde_json::Value, source_event: &str) -> String {
    extract_string(raw, &["title", "summary", "subject", "name"])
        .and_then(first_line)
        .unwrap_or_else(|| source_event.to_string())
}

fn extension_body_excerpt(raw: &serde_json::Value) -> Option<String> {
    extract_string(
        raw,
        &[
            "body",
            "message",
            "text",
            "description",
            "summary",
            "details",
        ],
    )
}

/// Extract human-readable text content for an observation body, accepting the
/// shapes agents actually send: a plain string, an **array of content blocks**
/// (`[{ "type": "text", "text": "…" }]` — the shape Claude Code uses for
/// `tool_response`), or a structured object (rendered as compact JSON). Unlike
/// [`extract_string`], which only matches a JSON string and silently drops
/// everything else, this keeps tool outputs / inputs that arrive as
/// arrays/objects.
fn extract_content(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        for candidate in extraction_candidates(value) {
            if let Some(found) = candidate.get(*key).and_then(value_to_text)
                && !found.is_empty()
            {
                return Some(found);
            }
        }
    }
    None
}

/// Flatten a JSON value into text. Strings pass through; arrays concatenate
/// their flattened items (one per line); objects prefer a `text` / `content`
/// field and otherwise fall back to compact JSON. `null` yields `None`.
fn value_to_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => (!s.is_empty()).then(|| s.clone()),
        serde_json::Value::Array(items) => {
            let joined = items
                .iter()
                .filter_map(value_to_text)
                .collect::<Vec<_>>()
                .join("\n");
            (!joined.is_empty()).then_some(joined)
        }
        serde_json::Value::Object(_) => value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
            .or_else(|| value.get("content").and_then(value_to_text))
            .or_else(|| {
                serde_json::to_string(value)
                    .ok()
                    .filter(|s| s != "{}" && s != "null")
            }),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null => None,
    }
}

fn best_body_excerpt(event: HookEvent, raw: &serde_json::Value) -> Option<String> {
    match event {
        HookEvent::UserPrompt => extract_content(raw, &["prompt", "message", "text"]),
        HookEvent::PostToolUse => {
            let tool = extract_string(raw, &["tool", "tool_name", "name"])
                .or_else(|| extract_string_path(raw, &[&["toolCall", "name"]]))
                .or_else(|| {
                    extract_scalar_string(raw, &["stepIdx"]).map(|step| format!("step {step}"))
                })?;
            let result =
                extract_content(raw, &["tool_response", "tool_output", "output", "result"])
                    .or_else(|| extract_content(raw, &["error"]))
                    .unwrap_or_else(|| "(no output captured)".into());
            Some(format!("tool: {tool}\n---\n{result}"))
        }
        HookEvent::Notification => extract_content(raw, &["message", "text"]),
        HookEvent::PostCompaction => extract_content(raw, &["summary"]),
        _ => None,
    }
}

/// Reduce a hook-supplied string to its first line.
///
/// Terminators are LF, CRLF, and a lone CR (`str::lines`). An empty first
/// line (a payload that starts with a newline) yields `None` so the caller
/// can fall back instead of storing `""`.
///
/// This is the only shaping `title_hint` gets before it reaches the
/// sanitizer: the 80-char display cap used to happen here too, but that ran
/// *before* redaction and could cut a secret in half — the truncated prefix
/// was then too short to match the sanitizer's pattern, so it was stored in
/// clear text (#980). The length cap now lives in
/// `ai_memory_core::sanitize::truncate_for_title`, applied by
/// `Sanitized::new` *after* `Sanitizer::scrub`.
fn first_line(s: String) -> Option<String> {
    // `str::lines` treats `\n`, `\r\n`, and a lone `\r` as terminators and
    // does not keep them. Stopping only at `\n` left a trailing CR on
    // Windows prompts, which the sanitizer preserves and `truncate_for_title`
    // does not strip, so the CR was stored in the observation title.
    let line = s.lines().next().unwrap_or("").to_string();
    (!line.is_empty()).then_some(line)
}

/// Cap core lifecycle body fields before the native hook writes its local
/// spool entry. The server independently reapplies the same per-event limits
/// while constructing [`HookEnvelope`], and the typed persistence boundary has
/// a universal backstop.
pub fn cap_lifecycle_body_for_client(raw: &mut serde_json::Value, event: HookEvent) -> bool {
    let Some((keys, max_bytes)) = core_body_cap(event) else {
        return false;
    };
    let mut changed = cap_candidate_group(raw, keys, max_bytes);
    for container in ["payload", "event"] {
        if let Some(nested) = raw.get_mut(container) {
            changed |= cap_candidate_group(nested, keys, max_bytes);
        }
    }
    changed
}

fn core_body_cap(event: HookEvent) -> Option<(&'static [&'static str], usize)> {
    match event {
        HookEvent::UserPrompt => Some((
            &["prompt", "message", "text"],
            USER_PROMPT_EXCERPT_MAX_BYTES,
        )),
        HookEvent::Notification => Some((&["message", "text"], NOTIFICATION_EXCERPT_MAX_BYTES)),
        HookEvent::PostCompaction => Some((&["summary"], POST_COMPACTION_EXCERPT_MAX_BYTES)),
        _ => None,
    }
}

fn cap_candidate_group(value: &mut serde_json::Value, keys: &[&str], max_bytes: usize) -> bool {
    let mut changed = cap_object_fields(value, keys, max_bytes);
    if let Some(properties) = value.get_mut("properties") {
        changed |= cap_object_fields(properties, keys, max_bytes);
        if let Some(info) = properties.get_mut("info") {
            changed |= cap_object_fields(info, keys, max_bytes);
        }
    }
    for container in ["info", "path"] {
        if let Some(nested) = value.get_mut(container) {
            changed |= cap_object_fields(nested, keys, max_bytes);
        }
    }
    changed
}

fn cap_object_fields(value: &mut serde_json::Value, keys: &[&str], max_bytes: usize) -> bool {
    let Some(object) = value.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for key in keys {
        let Some(field) = object.get_mut(*key) else {
            continue;
        };
        let Some(text) = value_to_text(field) else {
            continue;
        };
        if text.len() > max_bytes {
            // Scrub the full text before capping it. The client cannot see
            // the server's `[sanitize]` extras, but the built-in patterns
            // cover the credential classes and the server re-scrubs the
            // capped field with its configured sanitizer; truncating first
            // would cut a straddling secret in half and persist an
            // unmatchable prefix (#1114), the class #980 / #1109 fixed.
            // Same order as `assistant_capture::transform_for_client`.
            let scrubbed = Sanitizer::builtin().scrub(&text);
            *field = serde_json::Value::String(truncate_utf8_bytes(&scrubbed, max_bytes));
            changed = true;
        }
    }
    changed
}

fn normalize_extension_name(value: Option<&str>) -> Option<String> {
    normalize_token(value?, 64)
}

fn normalize_source_event(value: &str) -> Option<String> {
    normalize_token(value, 128)
}

fn normalize_token(value: &str, max_len: usize) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > max_len {
        return None;
    }
    if trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The predicate must accept exactly what `safe_tool_title` emits, for
    /// every `ToolFamily` variant. Inputs are built *through* the writer
    /// rather than written by hand, so the two cannot drift apart on what a
    /// family title looks like.
    ///
    /// The list below is checked by the compiler, not by trust: the
    /// wildcard-free `match` stops compiling the moment a variant is added,
    /// which routes whoever adds it here. Without that this test would keep
    /// passing while the new variant went uncovered — the comment would be
    /// making a promise the enumeration does not keep.
    #[test]
    fn the_predicate_recognises_every_title_the_writer_can_emit() {
        let every_variant = [
            ToolFamily::File,
            ToolFamily::SearchList,
            ToolFamily::NonFile,
            ToolFamily::Unknown,
        ];
        for family in every_variant {
            match family {
                ToolFamily::File
                | ToolFamily::SearchList
                | ToolFamily::NonFile
                | ToolFamily::Unknown => {}
            }
        }
        for family in every_variant {
            let written = safe_tool_title(&ToolObservationMetadata {
                tool_family: family,
                tool_call_id: None,
            });
            assert!(
                is_safe_tool_title(&written),
                "{written:?} is written by safe_tool_title and must be recognised"
            );
        }
    }

    /// A harness's own tool name is never mistaken for a family label, even
    /// when it happens to start with the same word.
    #[test]
    fn a_real_tool_name_is_not_a_family_label() {
        for name in [
            "Bash",
            "Edit",
            "tool",
            "tool ",
            "tool Bash",
            "toolfile",
            "Tool file",
        ] {
            assert!(
                !is_safe_tool_title(name),
                "{name:?} is not something safe_tool_title writes"
            );
        }
    }

    #[test]
    fn project_source_parses_both_spellings_and_defaults_closed() {
        assert_eq!(ProjectSource::parse(Some("marker")), ProjectSource::Marker);
        assert_eq!(
            ProjectSource::parse(Some("repo-root")),
            ProjectSource::RepoRoot
        );
        assert_eq!(
            ProjectSource::parse(Some("repo_root")),
            ProjectSource::RepoRoot
        );
        // Absent (older client) and unknown values both stay authoritative,
        // so a typo can never downgrade an override into something the
        // session is allowed to overrule.
        assert_eq!(ProjectSource::parse(None), ProjectSource::Unspecified);
        assert_eq!(
            ProjectSource::parse(Some("REPO-ROOT")),
            ProjectSource::Unspecified
        );
        assert_eq!(
            ProjectSource::parse(Some("nonsense")),
            ProjectSource::Unspecified
        );
        // Only a host-derived name may yield to the session.
        assert!(ProjectSource::RepoRoot.yields_to_session());
        assert!(!ProjectSource::Marker.yields_to_session());
        assert!(!ProjectSource::Unspecified.yields_to_session());
    }

    #[test]
    fn omitted_style_stays_legacy_host_path_while_new_clients_send_path() {
        let envelope = |identity_style: Option<&str>| {
            HookEnvelope::from_query_and_body(
                HookQuery {
                    event: "user-prompt-submit".into(),
                    identity_style: identity_style.map(str::to_owned),
                    ..Default::default()
                },
                serde_json::json!({ "session_id": "s" }),
            )
        };
        assert_eq!(
            envelope(None).identity_style,
            ai_memory_core::repository_identity::IdentityStyle::HostPath
        );
        assert_eq!(
            envelope(Some("path")).identity_style,
            ai_memory_core::repository_identity::IdentityStyle::Path
        );
        assert_eq!(
            envelope(Some("host_path")).identity_style,
            ai_memory_core::repository_identity::IdentityStyle::HostPath
        );
        assert_eq!(
            envelope(Some("unknown")).identity_style,
            ai_memory_core::repository_identity::IdentityStyle::HostPath
        );
    }

    #[test]
    fn project_source_is_pinned_unspecified_without_an_override() {
        // Provenance describes an override; a `project_src` arriving without
        // one is meaningless and must not read as a fell-through signal.
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt-submit".into(),
                agent: Some("claude-code".into()),
                cwd: Some("/checkouts/repo-a".into()),
                project_src: Some("repo-root".into()),
                ..Default::default()
            },
            serde_json::json!({ "session_id": "s" }),
        );
        assert_eq!(env.project_override, None);
        assert_eq!(env.project_source, ProjectSource::Unspecified);

        // With an override it is carried through verbatim.
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt-submit".into(),
                agent: Some("claude-code".into()),
                cwd: Some("/checkouts/repo-a".into()),
                project: Some("repo-a".into()),
                project_src: Some("repo-root".into()),
                ..Default::default()
            },
            serde_json::json!({ "session_id": "s" }),
        );
        assert_eq!(env.project_override.as_deref(), Some("repo-a"));
        assert_eq!(env.project_source, ProjectSource::RepoRoot);
    }

    /// `occurred_at` is client-controlled input arriving over the hook
    /// endpoint, so it must be bounded, not trusted outright. A valid past
    /// timestamp is stored exactly; garbage, non-positive, and far-future
    /// values must all fall back to `None` ("now" at the store boundary) —
    /// never an error, since hooks are fire-and-forget.
    #[test]
    fn occurred_at_micros_accepts_a_valid_past_timestamp() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt-submit".into(),
                ..Default::default()
            },
            serde_json::json!({ "session_id": "s", "occurred_at": "2026-09-10T12:00:00Z" }),
        );
        let expected = "2026-09-10T12:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_microsecond();
        assert_eq!(env.occurred_at_micros(), Some(expected));
    }

    #[test]
    fn occurred_at_micros_rejects_a_far_future_timestamp() {
        // Adversarial: a client claiming its event happened next year must not
        // be trusted — the bound must actually reject it.
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt-submit".into(),
                ..Default::default()
            },
            serde_json::json!({ "session_id": "s", "occurred_at": "2099-01-01T00:00:00Z" }),
        );
        assert_eq!(
            env.occurred_at_micros(),
            None,
            "a far-future timestamp must not be trusted"
        );
    }

    #[test]
    fn occurred_at_micros_rejects_non_positive_values() {
        for bogus in ["1970-01-01T00:00:00Z", "0000-01-01T00:00:00Z"] {
            let env = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: "user-prompt-submit".into(),
                    ..Default::default()
                },
                serde_json::json!({ "session_id": "s", "occurred_at": bogus }),
            );
            // The epoch and earlier parse fine as an RFC 3339 timestamp but
            // must still be rejected: a non-positive microsecond value is
            // never a plausible original event time.
            let micros = bogus.parse::<jiff::Timestamp>().unwrap().as_microsecond();
            assert!(micros <= 0, "fixture must actually be non-positive");
            assert_eq!(env.occurred_at_micros(), None);
        }
    }

    #[test]
    fn occurred_at_micros_rejects_garbage_strings() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt-submit".into(),
                ..Default::default()
            },
            serde_json::json!({ "session_id": "s", "occurred_at": "not-a-timestamp" }),
        );
        assert_eq!(env.occurred_at_micros(), None);
    }

    #[test]
    fn occurred_at_missing_from_the_body_parses_as_none() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt-submit".into(),
                ..Default::default()
            },
            serde_json::json!({ "session_id": "s" }),
        );
        assert_eq!(env.occurred_at, None);
        assert_eq!(env.occurred_at_micros(), None);
    }

    #[test]
    fn body_is_subagent_detects_harness_markers() {
        // grok tags subagent tool-use events with `subagentType`.
        assert!(body_is_subagent(
            &serde_json::json!({ "sessionId": "s", "subagentType": "general-purpose" })
        ));
        // Claude Code identifies spawned agents with `agent_id`.
        assert!(body_is_subagent(
            &serde_json::json!({ "session_id": "s", "agent_type": "workflow-subagent", "agent_id": "agent-abc123" })
        ));
        assert!(body_is_subagent(
            &serde_json::json!({ "agent_id": "agent-abc123" })
        ));
    }

    #[test]
    fn body_is_subagent_keeps_top_level_claude_agent_sessions() {
        // `claude --agent <name>` also sets agent_type on the main session.
        let mut raw = serde_json::json!({
            "session_id": "main-session",
            "agent_type": "regulus:regulus",
        });
        assert!(!body_is_subagent(&raw), "agent_id absent");
        for agent_id in [
            serde_json::Value::Null,
            serde_json::json!(""),
            serde_json::json!("   "),
            serde_json::json!(42),
            serde_json::json!(false),
        ] {
            raw["agent_id"] = agent_id;
            assert!(!body_is_subagent(&raw), "{raw}");
        }
    }

    #[test]
    fn body_is_subagent_false_for_top_level_and_empty_markers() {
        // A normal top-level event carries no marker.
        assert!(!body_is_subagent(
            &serde_json::json!({ "session_id": "s", "tool_name": "Write" })
        ));
        // An empty / blank or non-string marker does not count as a subagent.
        assert!(!body_is_subagent(
            &serde_json::json!({ "subagentType": "" })
        ));
        assert!(!body_is_subagent(
            &serde_json::json!({ "subagentType": "   " })
        ));
        assert!(!body_is_subagent(
            &serde_json::json!({ "agent_type": null })
        ));
        assert!(!body_is_subagent(&serde_json::json!({})));
    }

    #[test]
    fn parses_known_events() {
        assert_eq!(HookEvent::parse("session-start"), HookEvent::SessionStart);
        assert_eq!(HookEvent::parse("PreToolUse"), HookEvent::PreToolUse);
        assert_eq!(HookEvent::parse("user_prompt"), HookEvent::UserPrompt);
        assert_eq!(HookEvent::parse("bogus"), HookEvent::Other);
    }

    #[test]
    fn debug_never_renders_hook_content() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt".into(),
                agent: Some("claude-code".into()),
                managed_run: Some("run-1".into()),
                ..Default::default()
            },
            serde_json::json!({
                "session_id": "dbg",
                "prompt": "SENTINEL_DERIVED_CONTENT",
                "secret": "SENTINEL_RAW_PAYLOAD",
            }),
        );
        assert_eq!(
            env.body_excerpt.as_deref(),
            Some("SENTINEL_DERIVED_CONTENT")
        );
        let rendered = format!("{env:?}");
        assert!(
            !rendered.contains("SENTINEL_RAW_PAYLOAD"),
            "Debug leaked the raw payload: {rendered}"
        );
        assert!(
            !rendered.contains("SENTINEL_DERIVED_CONTENT"),
            "Debug leaked derived hook content: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "raw was not redacted");
        assert!(rendered.contains("UserPrompt"), "event field went missing");
        assert!(rendered.contains("run-1"), "managed run field went missing");
    }

    /// `ingest_key` is client-controlled input: only short plain tokens pass;
    /// anything else degrades to "absent" (at-least-once), never a 4xx.
    #[test]
    fn ingest_key_is_validated_at_parse_time() {
        let fire = |key: Option<&str>| {
            HookEnvelope::from_query_and_body(
                HookQuery {
                    event: "stop".into(),
                    ingest_key: key.map(str::to_string),
                    ..Default::default()
                },
                serde_json::json!({ "session_id": "k-1" }),
            )
        };
        // A UUID in simple or hyphenated form passes untouched.
        assert_eq!(
            fire(Some("abcDEF123_-")).ingest_key.as_deref(),
            Some("abcDEF123_-")
        );
        // Empty, oversized or non-token input is dropped, not rejected.
        let oversized = "x".repeat(65);
        for bad in ["", "spaces here", "chave!", "key\n", oversized.as_str()] {
            assert_eq!(
                fire(Some(bad)).ingest_key,
                None,
                "expected {bad:?} to be dropped"
            );
        }
        assert_eq!(fire(None).ingest_key, None);
    }

    #[test]
    fn extension_event_preserves_source_event_when_opted_in() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "lead.contact".into(),
                extension: Some("fstech".into()),
                ..Default::default()
            },
            serde_json::json!({
                "session_id": "fst-1",
                "title": "Lead contacted",
                "message": "Lead Maria requested a proposal"
            }),
        );

        assert_eq!(env.event, HookEvent::Other);
        assert_eq!(env.extension.as_deref(), Some("fstech"));
        assert_eq!(env.source_event.as_deref(), Some("lead.contact"));
        assert_eq!(env.title_hint.as_deref(), Some("Lead contacted"));
        assert_eq!(
            env.body_excerpt.as_deref(),
            Some("Lead Maria requested a proposal")
        );
    }

    #[test]
    fn unknown_event_without_extension_leaves_no_source_event() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "lead.contact".into(),
                ..Default::default()
            },
            serde_json::json!({
                "session_id": "fst-1",
                "title": "Lead contacted",
                "message": "Lead Maria requested a proposal"
            }),
        );

        assert_eq!(env.event, HookEvent::Other);
        assert_eq!(env.extension, None);
        assert_eq!(env.source_event, None);
        assert_eq!(env.title_hint, None);
        assert_eq!(env.body_excerpt, None);
    }

    #[test]
    fn invalid_extension_tokens_are_not_preserved() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "lead.contact".into(),
                extension: Some("bad extension".into()),
                ..Default::default()
            },
            serde_json::json!({ "session_id": "fst-1" }),
        );

        assert_eq!(env.extension, None);
        assert_eq!(env.source_event, None);
    }

    #[test]
    fn maps_to_observation_kind() {
        assert_eq!(
            HookEvent::SessionEnd.to_observation_kind(),
            ObservationKind::SessionEnd
        );
        assert_eq!(
            HookEvent::PostCompaction.to_observation_kind(),
            ObservationKind::PostCompaction
        );
    }

    #[test]
    fn hook_event_parses_post_compaction() {
        assert_eq!(
            HookEvent::parse("post-compaction"),
            HookEvent::PostCompaction
        );
        assert_eq!(
            HookEvent::parse("post_compaction"),
            HookEvent::PostCompaction
        );
        assert_eq!(
            HookEvent::parse("PostCompaction"),
            HookEvent::PostCompaction
        );
        // Unknown event still maps to Other.
        assert_eq!(HookEvent::parse("unknown-event"), HookEvent::Other);
    }

    #[test]
    fn envelope_maps_default_global_flag() {
        let on = HookEnvelope::from_query_and_body(
            HookQuery {
                default_global: Some("true".into()),
                ..Default::default()
            },
            serde_json::json!({ "session_id": "s1" }),
        );
        assert!(on.recall_default_global_requested);

        let off = HookEnvelope::from_query_and_body(
            HookQuery::default(),
            serde_json::json!({ "session_id": "s1" }),
        );
        assert!(!off.recall_default_global_requested);
    }

    #[test]
    fn envelope_extracts_session_and_cwd() {
        let q = HookQuery {
            event: "session-start".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "session_id": "abc-123",
            "cwd": "/tmp/x",
            "model": "claude-sonnet-4-6"
        });
        let env = HookEnvelope::from_query_and_body(q.clone(), raw);
        assert_eq!(env.event, HookEvent::SessionStart);
        assert_eq!(env.session_id.as_deref(), Some("abc-123"));
        assert_eq!(env.cwd.as_deref(), Some("/tmp/x"));
        assert_eq!(env.title_hint.as_deref(), Some("claude-sonnet-4-6"));
    }

    /// Cursor's `sessionStart` carries no `cwd` key at all — the workspace
    /// directory arrives only as `workspace_roots`. Shape captured live from
    /// Cursor CLI 2026.09.02-c22c1a3.
    #[test]
    fn envelope_resolves_cursor_session_start_cwd_from_workspace_roots() {
        let q = HookQuery {
            event: "session-start".into(),
            agent: Some("cursor".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "conversation_id": "cf111450-8c45-4da1-a384-7a48e08099c3",
            "session_id": "cf111450-8c45-4da1-a384-7a48e08099c3",
            "is_background_agent": false,
            "hook_event_name": "sessionStart",
            "cursor_version": "2026.09.02-c22c1a3",
            "workspace_roots": ["/checkouts/repo-a"],
            "transcript_path": serde_json::Value::Null
        });

        let env = HookEnvelope::from_query_and_body(q, raw);

        assert_eq!(env.event, HookEvent::SessionStart);
        assert_eq!(env.agent, AgentKind::Cursor);
        assert_eq!(
            env.cwd.as_deref(),
            Some("/checkouts/repo-a"),
            "without workspace_roots the session resolves to no cwd and lands \
             in the server-default scratch project"
        );
    }

    /// Cursor's tool events DO carry a `cwd` key, but send it as an empty
    /// string; resolution must fall through to `workspace_roots` instead of
    /// accepting `""`.
    #[test]
    fn envelope_resolves_cursor_tool_event_cwd_despite_empty_cwd_string() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("cursor".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "conversation_id": "cf111450-8c45-4da1-a384-7a48e08099c3",
            "session_id": "cf111450-8c45-4da1-a384-7a48e08099c3",
            "tool_name": "Shell",
            "tool_input": {"command": "echo cap > CAP.txt", "cwd": "", "timeout": 30000},
            "tool_use_id": "b0fe7c49-7ee7-45e6-91ee-6ee68b7b17e2",
            "cwd": "",
            "hook_event_name": "postToolUse",
            "cursor_version": "2026.09.02-c22c1a3",
            "workspace_roots": ["/checkouts/repo-a"]
        });

        let env = HookEnvelope::from_query_and_body(q, raw);

        assert_eq!(env.cwd.as_deref(), Some("/checkouts/repo-a"));
    }

    /// The Cursor CLI also runs the hook commands declared in Claude Code's
    /// `~/.claude/settings.json`, which `install-hooks --agent claude-code`
    /// hardcoded to `--agent claude-code`. The payload's `cursor_version`
    /// identifies the real harness, so the session must not be filed as
    /// Claude Code.
    #[test]
    fn envelope_attributes_cursor_payload_to_cursor_over_declared_claude_code() {
        let q = HookQuery {
            event: "session-start".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "conversation_id": "cf111450-8c45-4da1-a384-7a48e08099c3",
            "session_id": "cf111450-8c45-4da1-a384-7a48e08099c3",
            "hook_event_name": "sessionStart",
            "cursor_version": "2026.09.02-c22c1a3",
            "workspace_roots": ["/checkouts/repo-a"]
        });

        let env = HookEnvelope::from_query_and_body(q, raw);

        assert_eq!(env.agent, AgentKind::Cursor);
        assert_eq!(env.cwd.as_deref(), Some("/checkouts/repo-a"));
    }

    /// A genuine Claude Code payload carries no vendor marker, so the
    /// declared `?agent=` still decides.
    #[test]
    fn envelope_keeps_declared_agent_when_payload_has_no_vendor_marker() {
        let q = HookQuery {
            event: "session-start".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "session_id": "abc-123",
            "cwd": "/checkouts/repo-a",
            "hook_event_name": "SessionStart"
        });

        let env = HookEnvelope::from_query_and_body(q, raw);

        assert_eq!(env.agent, AgentKind::ClaudeCode);
    }

    #[test]
    fn envelope_uses_query_session_id_when_body_omits_it() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("devin".into()),
            session_id: Some("bridge-session-123".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "exec",
            "tool_input": {"command": "ls"},
            "tool_use_id": "call_c101a272288d400b831e1498",
            "tool_response": {"success": true, "output": "ok", "error": null}
        });

        let env = HookEnvelope::from_query_and_body(q.clone(), raw);

        assert_eq!(env.event, HookEvent::PostToolUse);
        assert_eq!(env.agent, AgentKind::Devin);
        assert_eq!(env.session_id.as_deref(), Some("bridge-session-123"));
    }

    #[test]
    fn envelope_body_session_id_wins_over_query_session_id() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("devin".into()),
            session_id: Some("bridge-session-123".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "session_id": "body-session-456",
            "hook_event_name": "PostToolUse",
            "tool_name": "exec"
        });

        let env = HookEnvelope::from_query_and_body(q, raw);

        assert_eq!(env.session_id.as_deref(), Some("body-session-456"));
    }

    #[test]
    fn envelope_uses_query_cwd_when_body_omits_it() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("devin".into()),
            cwd: Some("/resolved/from/hook".into()),
            session_id: Some("bridge-session-123".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "exec",
            "tool_input": {"command": "ls"},
            "tool_use_id": "call_c101a272288d400b831e1498",
            "tool_response": {"success": true, "output": "ok", "error": null}
        });

        let env = HookEnvelope::from_query_and_body(q, raw);

        assert_eq!(env.agent, AgentKind::Devin);
        assert_eq!(env.session_id.as_deref(), Some("bridge-session-123"));
        assert_eq!(env.cwd.as_deref(), Some("/resolved/from/hook"));
    }

    #[test]
    fn envelope_body_cwd_wins_over_query_cwd() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("devin".into()),
            cwd: Some("/resolved/from/hook".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "cwd": "/native/from/body"
        });

        let env = HookEnvelope::from_query_and_body(q, raw);

        assert_eq!(env.cwd.as_deref(), Some("/native/from/body"));
    }

    #[test]
    fn devin_real_session_start_fixture_has_no_native_session_or_cwd() {
        let q = HookQuery {
            event: "session-start".into(),
            agent: Some("devin".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "source": "startup"
        });
        let env = HookEnvelope::from_query_and_body(q, raw);
        assert_eq!(env.event, HookEvent::SessionStart);
        assert_eq!(env.agent, AgentKind::Devin);
        assert!(env.session_id.is_none());
        assert!(env.cwd.is_none());
    }

    #[test]
    fn envelope_parses_project_strategy_query() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "session-start".into(),
                project_strategy: Some("repo-root".into()),
                ..Default::default()
            },
            serde_json::json!({}),
        );

        assert_eq!(env.project_strategy, ProjectStrategy::RepoRoot);
    }

    /// Antigravity CLI identifies the conversation as `conversationId`
    /// and reports cwd-like routing through `workspacePaths`.
    #[test]
    fn envelope_extracts_antigravity_conversation_and_workspace_path() {
        let q = HookQuery {
            event: "PreToolUse".into(),
            agent: Some("agy".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
            "workspacePaths": ["/workspace/project", "/workspace/other"],
            "toolCall": {
                "name": "run_command",
                "args": {"CommandLine": "cargo test"}
            },
            "stepIdx": 3
        });
        let env = HookEnvelope::from_query_and_body(q, raw);

        assert_eq!(env.agent, AgentKind::AntigravityCli);
        assert_eq!(env.event, HookEvent::PreToolUse);
        assert_eq!(
            env.session_id.as_deref(),
            Some("ec33ebf9-0cba-4100-8142-c61503f6c587")
        );
        assert_eq!(env.cwd.as_deref(), Some("/workspace/project"));
        assert_eq!(env.title_hint.as_deref(), Some("tool non-file"));
    }

    #[test]
    fn envelope_uses_antigravity_step_idx_for_post_tool_title_fallback() {
        let q = HookQuery {
            event: "PostToolUse".into(),
            agent: Some("antigravity-cli".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "conversationId": "agy-conv",
            "workspacePaths": ["/workspace/project"],
            "stepIdx": 5,
            "error": "exit status 1"
        });
        let env = HookEnvelope::from_query_and_body(q, raw);
        assert!(env.title_hint.is_none());
        assert!(env.body_excerpt.is_none());
    }

    /// OpenCode's plugin SDK sends `sessionID` (capital `ID`) on the
    /// tool.execute.* / session.* events. Regression for issue #1: this
    /// spelling must be extracted, otherwise non-session-start events
    /// fail the router's "missing session_id" check.
    #[test]
    fn envelope_extracts_opencode_camelcase_session_id() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("open-code".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "sessionID": "ses_abc123",
            "tool": "bash",
            "callID": "call_1"
        });
        let env = HookEnvelope::from_query_and_body(q, raw);
        assert_eq!(env.session_id.as_deref(), Some("ses_abc123"));
    }

    /// Earlier OpenCode plugin generation wrapped the actual SDK hook
    /// input under `payload`. Keep accepting that shape so users with
    /// an old plugin don't silently lose project routing until they
    /// restart with the fixed plugin.
    #[test]
    fn envelope_extracts_legacy_opencode_nested_payload() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("open-code".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "hook_event_name": "post-tool-use",
            "agent": "open-code",
            "payload": {
                "sessionID": "ses_nested",
                "cwd": "/home/user/ai-memory",
                "tool": "bash",
                "output": "tests passed"
            }
        });
        let env = HookEnvelope::from_query_and_body(q.clone(), raw);
        assert_eq!(env.session_id.as_deref(), Some("ses_nested"));
        assert_eq!(env.cwd.as_deref(), Some("/home/user/ai-memory"));
        assert_eq!(env.title_hint.as_deref(), Some("bash"));
        assert_eq!(
            env.body_excerpt.as_deref(),
            Some("tool: bash\n---\ntests passed")
        );
        let long = HookEnvelope::from_query_and_body(
            q,
            serde_json::json!({"payload":{"tool":"bash","output":"é".repeat(2_000)}}),
        );
        let body = long.body_excerpt.unwrap();
        assert!(body.len() > 2_000);
        let capped = truncate_utf8_bytes(&body, durable_body_cap(HookEvent::PostToolUse));
        assert!(capped.len() <= 2_000);
    }

    /// OpenCode's plugin `event` hook receives bus events shaped like
    /// `{ event: { type, properties } }`; session creation carries the
    /// cwd as `properties.info.directory`.
    #[test]
    fn envelope_extracts_opencode_bus_event_session_info() {
        let q = HookQuery {
            event: "session-start".into(),
            agent: Some("open-code".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "event": {
                "type": "session.created",
                "properties": {
                    "sessionID": "ses_bus",
                    "info": {
                        "id": "ses_bus",
                        "directory": "/home/user/ai-memory",
                        "title": "New session"
                    }
                }
            }
        });
        let env = HookEnvelope::from_query_and_body(q, raw);
        assert_eq!(env.session_id.as_deref(), Some("ses_bus"));
        assert_eq!(env.cwd.as_deref(), Some("/home/user/ai-memory"));
        assert_eq!(env.title_hint.as_deref(), Some("New session"));
    }

    /// Alternative agent-name spellings all map to the same canonical
    /// AgentKind. The hook scripts and the test e2e shim send slightly
    /// different strings for historical reasons; this asserts we
    /// remain forgiving.
    #[test]
    fn agent_name_aliases_all_map_correctly() {
        assert_eq!(parse_agent("claude-code"), AgentKind::ClaudeCode);
        assert_eq!(parse_agent("claude_code"), AgentKind::ClaudeCode);
        assert_eq!(parse_agent("claude"), AgentKind::ClaudeCode);
        assert_eq!(parse_agent("codex"), AgentKind::Codex);
        assert_eq!(parse_agent("opencode"), AgentKind::OpenCode);
        assert_eq!(parse_agent("open-code"), AgentKind::OpenCode);
        assert_eq!(parse_agent("cursor"), AgentKind::Cursor);
        assert_eq!(parse_agent("gemini-cli"), AgentKind::GeminiCli);
        assert_eq!(parse_agent("gemini"), AgentKind::GeminiCli);
        assert_eq!(parse_agent("claude-desktop"), AgentKind::ClaudeDesktop);
        assert_eq!(parse_agent("openclaw"), AgentKind::OpenClaw);
        assert_eq!(parse_agent("antigravity-cli"), AgentKind::AntigravityCli);
        assert_eq!(parse_agent("antigravity"), AgentKind::AntigravityCli);
        assert_eq!(parse_agent("agy"), AgentKind::AntigravityCli);
        assert_eq!(parse_agent("omp"), AgentKind::Omp);
        assert_eq!(parse_agent("pi"), AgentKind::Pi);
        assert_eq!(parse_agent("oh-my-pi"), AgentKind::Omp);
        assert_eq!(parse_agent("hermes"), AgentKind::Hermes);
        assert_eq!(parse_agent("hermes-agent"), AgentKind::Hermes);
        assert_eq!(parse_agent("pool"), AgentKind::Pool);
        assert_eq!(parse_agent("poolside"), AgentKind::Pool);
        // Anything else is `Other`. Critical for the hook router:
        // a typo in the query string must not crash, it just gets
        // attributed to the catch-all bucket.
        assert_eq!(parse_agent(""), AgentKind::Other);
        assert_eq!(parse_agent("CLAUDE-CODE"), AgentKind::Other); // case-sensitive on purpose
        assert_eq!(parse_agent("../../etc/passwd"), AgentKind::Other);
    }

    /// An empty body is legitimate (some hook events carry no
    /// payload). Envelope extraction must produce sane defaults
    /// rather than panicking.
    #[test]
    fn envelope_tolerates_empty_body() {
        let q = HookQuery {
            event: "stop".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(q, serde_json::json!({}));
        assert_eq!(env.event, HookEvent::Stop);
        assert!(env.session_id.is_none());
        assert!(env.cwd.is_none());
        assert!(env.title_hint.is_none());
        assert!(env.body_excerpt.is_none());
    }

    #[test]
    fn hermes_tool_title_uses_only_the_verified_shell_hook_shape() {
        let raw = serde_json::json!({
            "hook_event_name": "post_tool_call",
            "tool_name": "write_file",
            "tool_input": {"path": "src/lib.rs", "content": "untrusted"},
            "session_id": "hermes-session",
            "cwd": "/repo",
            "extra": {"tool_call_id": "call-42", "status": "ok"}
        });
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("hermes".into()),
                ..Default::default()
            },
            raw.clone(),
        );
        assert_eq!(env.agent, AgentKind::Hermes);
        assert_eq!(env.title_hint.as_deref(), Some("tool file"));
        assert_eq!(env.session_id.as_deref(), Some("hermes-session"));
        assert_eq!(env.cwd.as_deref(), Some("/repo"));

        let unknown = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("unverified-agent".into()),
                ..Default::default()
            },
            raw,
        );
        assert_eq!(unknown.agent, AgentKind::Other);
        assert!(unknown.title_hint.is_none());
    }

    /// Hermes 3.x puts the tool result inside `extra.result` and mirrors the
    /// call status in `extra.status` (`ok` | `error`). The extractor must read
    /// both from the documented `extra` object: output lands in the body
    /// instead of "(no output captured)", and a proven status maps to a real
    /// outcome instead of permanent `unknown`. Live-captured 2026-10-07 from
    /// `agent/shell_hooks.py::_payload_fields` +
    /// `model_tools.py::_emit_post_tool_call_hook` (Hermes main).
    #[test]
    fn hermes_output_and_outcome_come_from_the_extra_object() {
        let raw = serde_json::json!({
            "hook_event_name": "post_tool_call",
            "tool_name": "execute_code",
            "tool_input": {"code": "print(1)"},
            "session_id": "hermes-session",
            "cwd": "/repo",
            "extra": {
                "tool_call_id": "call-43",
                "status": "ok",
                "result": {"output": "1", "exit_code": 0}
            }
        });
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("hermes".into()),
                ..Default::default()
            },
            raw,
        );
        let body = env.body_excerpt.expect("post-tool-use body");
        assert!(body.contains("tool_family: non-file"), "body: {body}");
        assert_eq!(env.title_hint.as_deref(), Some("tool non-file"));
        assert!(
            body.contains("outcome: success"),
            "status ok must map to success — body: {body}"
        );
        assert!(
            body.contains("\"output\": \"1\"") || body.contains("exit_code"),
            "extra.result content must appear in the body — body: {body}"
        );
    }

    #[test]
    fn hermes_error_status_maps_to_error_outcome() {
        let raw = serde_json::json!({
            "hook_event_name": "post_tool_call",
            "tool_name": "terminal",
            "tool_input": {"command": "false"},
            "session_id": "hermes-session",
            "cwd": "/repo",
            "extra": {
                "tool_call_id": "call-44",
                "status": "error",
                "error_message": "command failed with exit code 1",
                "result": "command failed"
            }
        });
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("hermes".into()),
                ..Default::default()
            },
            raw,
        );
        let body = env.body_excerpt.expect("post-tool-use body");
        assert!(body.contains("outcome: error"), "body: {body}");
    }

    /// A truly unknown tool name keeps the upstream privacy invariant: the
    /// family gates output capture, so `unknown` still omits the body. Real
    /// Hermes tools that execute code or reach the web are named in
    /// `family()` and classify as `non-file` (see
    /// `hermes_documented_tool_names_map_to_canonical_families`); a brand-new
    /// Hermes tool degrades to metadata-only until its name is upstreamed —
    /// capture fidelity is preserved by extending the list, not by relaxing
    /// the unknown-family gate.
    #[test]
    fn hermes_unknown_family_keeps_metadata_only_invariant() {
        let raw = serde_json::json!({
            "hook_event_name": "post_tool_call",
            "tool_name": "brand_new_tool",
            "tool_input": {"query": "x"},
            "session_id": "hermes-session",
            "cwd": "/repo",
            "extra": {
                "tool_call_id": "call-45",
                "status": "ok",
                "result": "search hit count: 3"
            }
        });
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("hermes".into()),
                ..Default::default()
            },
            raw,
        );
        let body = env.body_excerpt.expect("post-tool-use body");
        assert!(body.contains("tool_family: unknown"), "body: {body}");
        assert!(body.contains("outcome: success"), "body: {body}");
        assert!(
            !body.contains("search hit count: 3"),
            "unknown family must not leak output — body: {body}"
        );
    }

    /// Real Hermes execution-surface tools classify as non-file so their
    /// bodies carry the `extra.result` output (live names, 2026-10-07).
    #[test]
    fn hermes_execution_tools_classify_non_file_and_capture_output() {
        for tool in ["execute_code", "browser_exec", "web_extract"] {
            let raw = serde_json::json!({
                "hook_event_name": "post_tool_call",
                "tool_name": tool,
                "tool_input": {"query": "x"},
                "session_id": "hermes-session",
                "cwd": "/repo",
                "extra": {
                    "tool_call_id": "call-46",
                    "status": "ok",
                    "result": "CAPTURE-ME-42"
                }
            });
            let env = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: "post-tool-use".into(),
                    agent: Some("hermes".into()),
                    ..Default::default()
                },
                raw,
            );
            let body = env.body_excerpt.unwrap_or_default();
            assert!(
                body.contains("tool_family: non-file"),
                "{tool} should be non-file — body: {body}"
            );
            assert!(
                body.contains("CAPTURE-ME-42"),
                "{tool} output must be captured — body: {body}"
            );
        }
    }

    #[test]
    fn pool_tool_title_uses_the_documented_snake_case_shape() {
        let raw = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "edit",
            "tool_input": {"path": "src/lib.rs", "old_string": "a", "new_string": "b"},
            "session_id": "pool-session",
            "cwd": "/repo"
        });
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("pool".into()),
                ..Default::default()
            },
            raw,
        );
        assert_eq!(env.agent, AgentKind::Pool);
        assert_eq!(env.title_hint.as_deref(), Some("tool file"));
        assert_eq!(env.session_id.as_deref(), Some("pool-session"));
        assert_eq!(env.cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn zcode_tool_payload_uses_the_captured_dual_casing_shape() {
        // Live-captured PostToolUse payload (embedded engine v0.16.5, #512):
        // native camelCase plus Claude Code-compatible snake_case aliases on
        // every shared field.
        let raw = serde_json::json!({
            "hookEventName": "PostToolUse",
            "hook_event_name": "PostToolUse",
            "toolName": "Bash",
            "tool_name": "Bash",
            "toolCallId": "call_86cedcb9c40a4a40856e9ee7",
            "tool_use_id": "call_86cedcb9c40a4a40856e9ee7",
            "toolInput": {"command": "echo capture-ok"},
            "tool_input": {"command": "echo capture-ok"},
            "sessionId": "sess_4fc06da3",
            "session_id": "sess_4fc06da3",
            "cwd": "/tmp/zcode-capture"
        });
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("zcode".into()),
                ..Default::default()
            },
            raw,
        );
        assert_eq!(env.agent, AgentKind::Zcode);
        assert_eq!(env.session_id.as_deref(), Some("sess_4fc06da3"));
        assert_eq!(env.cwd.as_deref(), Some("/tmp/zcode-capture"));
        assert_eq!(env.title_hint.as_deref(), Some("tool non-file"));
    }

    #[test]
    fn grizzybot_tool_title_is_a_closed_family() {
        let raw = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "write_file",
            "tool_input": {"path": "notes.md", "content": "untrusted"},
            "tool_response": "ok",
            "session_id": "gb-session",
            "cwd": "/bot/home"
        });
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("grizzybot".into()),
                ..Default::default()
            },
            raw,
        );
        assert_eq!(env.agent, AgentKind::Grizzybot);
        assert_eq!(env.title_hint.as_deref(), Some("tool file"));
        assert_eq!(env.session_id.as_deref(), Some("gb-session"));
        assert_eq!(env.cwd.as_deref(), Some("/bot/home"));
    }

    /// Body is well-formed JSON but the expected `session_id` /
    /// `cwd` keys are missing — extraction returns None per key.
    #[test]
    fn envelope_missing_expected_fields() {
        let q = HookQuery {
            event: "user-prompt".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({ "garbage": 42 });
        let env = HookEnvelope::from_query_and_body(q, raw);
        assert_eq!(env.event, HookEvent::UserPrompt);
        assert!(env.session_id.is_none());
        assert!(env.cwd.is_none());
    }

    /// Body is a JSON primitive (string / null / number) rather
    /// than an object. The extractors must short-circuit cleanly.
    /// This guards against an upstream that POSTs a stringified
    /// payload by mistake.
    #[test]
    fn envelope_accepts_non_object_body() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        for raw in [
            serde_json::json!(null),
            serde_json::json!("a stringy payload"),
            serde_json::json!(42),
            serde_json::json!([1, 2, 3]),
        ] {
            let env = HookEnvelope::from_query_and_body(q.clone(), raw);
            assert!(
                env.session_id.is_none(),
                "no session_id from non-object body"
            );
            assert!(env.cwd.is_none(), "no cwd from non-object body");
        }
    }

    /// Empty `agent` query param maps to Other (rather than panic
    /// or default to ClaudeCode). The hook router uses this for the
    /// attribution column, so we want it consistent.
    #[test]
    fn missing_agent_query_param_maps_to_other() {
        let q = HookQuery {
            event: "session-end".into(),
            agent: None,
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(q, serde_json::json!({}));
        assert_eq!(env.agent, AgentKind::Other);
    }

    /// Title-hint extraction must truncate at the first newline (the
    /// "first line" rule used everywhere in the wiki log + handoff
    /// surfaces), but must NOT cap length here any more: the 80-char display
    /// cap moved to `ai_memory_core::sanitize::truncate_for_title`, applied
    /// by `Sanitized::new` after redaction (#980) — truncating a long
    /// `title_hint` in `payload.rs`, before the sanitizer ever runs, could
    /// cut a secret in half and leave the unmatched fragment unredacted.
    #[test]
    fn user_prompt_title_keeps_first_line_full_length_untruncated() {
        let q = HookQuery {
            event: "user-prompt".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        // Multi-line prompt → title is the first line only.
        let env = HookEnvelope::from_query_and_body(
            q.clone(),
            serde_json::json!({ "prompt": "first line\nsecond line should be lost" }),
        );
        assert_eq!(env.title_hint.as_deref(), Some("first line"));

        let env = HookEnvelope::from_query_and_body(
            q.clone(),
            serde_json::json!({ "prompt": "first line\r\nsecond line should be lost" }),
        );
        assert_eq!(
            env.title_hint.as_deref(),
            Some("first line"),
            "CRLF must not leave a trailing CR on the title"
        );

        let env = HookEnvelope::from_query_and_body(
            q.clone(),
            serde_json::json!({ "prompt": "\r\nhello" }),
        );
        assert!(
            env.title_hint.is_none(),
            "empty first line after leading CRLF must not become a CR or empty title, got {:?}",
            env.title_hint
        );

        // Very long single line → title_hint keeps every character; the
        // 80-char cap now happens later, after sanitization.
        let long = "x".repeat(200);
        let env = HookEnvelope::from_query_and_body(q, serde_json::json!({ "prompt": long }));
        let title = env.title_hint.unwrap();
        assert_eq!(title.chars().count(), 200);
        assert!(!title.contains('…'));
    }

    #[test]
    fn notification_and_compaction_titles_are_the_first_line() {
        let start = HookQuery {
            event: "session-start".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(
            start,
            serde_json::json!({ "title": "first\r\nsecond" }),
        );
        assert_eq!(env.title_hint.as_deref(), Some("first"));

        let notify = HookQuery {
            event: "notification".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(
            notify,
            serde_json::json!({ "message": "first\r\nsecond" }),
        );
        assert_eq!(env.title_hint.as_deref(), Some("first"));

        let compact = HookQuery {
            event: "post-compaction".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(
            compact,
            serde_json::json!({ "summary": "kept\nlost" }),
        );
        assert_eq!(env.title_hint.as_deref(), Some("kept"));
    }

    #[test]
    fn core_lifecycle_bodies_use_named_utf8_safe_caps() {
        for (event, field, cap) in [
            ("user-prompt", "prompt", USER_PROMPT_EXCERPT_MAX_BYTES),
            ("notification", "message", NOTIFICATION_EXCERPT_MAX_BYTES),
            (
                "post-compaction",
                "summary",
                POST_COMPACTION_EXCERPT_MAX_BYTES,
            ),
        ] {
            let body = format!("{}éTAIL_SENTINEL", "x".repeat(cap - 1));
            let env = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: event.into(),
                    agent: Some("claude-code".into()),
                    ..Default::default()
                },
                serde_json::json!({(field): body}),
            );
            // Extraction no longer caps (#1114): the sanitizer at the ingest
            // funnel must see the full text before any byte limit runs. The
            // scrub-then-cap order itself is asserted by the router's
            // `issue_1114_*` tests.
            let excerpt = env.body_excerpt.expect("body excerpt");
            assert_eq!(excerpt, body, "{event} excerpt was capped at extraction");
            let hook_event = HookEvent::parse(event);
            assert_eq!(durable_body_cap(hook_event), cap);
            let capped = truncate_utf8_bytes(&excerpt, durable_body_cap(hook_event));
            assert!(capped.len() <= cap, "{event} exceeded {cap} bytes");
            assert!(capped.ends_with('…'), "{event} omitted truncation marker");
            assert!(
                !capped.contains("TAIL_SENTINEL"),
                "{event} retained content after the cap"
            );
        }
    }

    #[test]
    fn client_body_cap_covers_supported_nested_candidate_shapes() {
        let mut raw = serde_json::json!({
            "prompt": "x".repeat(USER_PROMPT_EXCERPT_MAX_BYTES + 1),
            "payload": {
                "properties": {
                    "info": {
                        "message": [{"type": "text", "text": "y".repeat(USER_PROMPT_EXCERPT_MAX_BYTES + 1)}]
                    }
                }
            },
            "event": {
                "info": {
                    "text": "z".repeat(USER_PROMPT_EXCERPT_MAX_BYTES + 1)
                }
            }
        });
        assert!(cap_lifecycle_body_for_client(
            &mut raw,
            HookEvent::UserPrompt
        ));
        for value in [
            &raw["prompt"],
            &raw["payload"]["properties"]["info"]["message"],
            &raw["event"]["info"]["text"],
        ] {
            let text = value.as_str().expect("oversized value flattened to text");
            assert!(text.len() <= USER_PROMPT_EXCERPT_MAX_BYTES);
            assert!(text.ends_with('…'));
        }
        assert!(!cap_lifecycle_body_for_client(
            &mut raw,
            HookEvent::UserPrompt
        ));
        assert!(!cap_lifecycle_body_for_client(
            &mut raw,
            HookEvent::SessionStart
        ));
    }

    /// Regression for #1114 (client side): an oversized lifecycle body is
    /// scrubbed with the built-in sanitizer **before** the spool cap runs, so
    /// a secret straddling the cutoff is redacted whole instead of being cut
    /// into an unmatchable prefix that the spool — and later the server —
    /// persists in clear text. Mirrors `transform_for_client` (#196) and the
    /// #980 / #1109 scrub-before-cap fixes.
    #[test]
    fn client_body_cap_scrubs_before_truncating() {
        // AWS access key id shape: a built-in pattern.
        let secret = format!("AKIA{}", "A".repeat(16));
        let mut raw = serde_json::json!({
            "prompt": format!("{} {secret}", "x".repeat(USER_PROMPT_EXCERPT_MAX_BYTES - 14)),
        });
        assert!(cap_lifecycle_body_for_client(
            &mut raw,
            HookEvent::UserPrompt
        ));
        let capped = raw["prompt"].as_str().expect("oversized prompt capped");
        assert!(
            !capped.contains("AKIA"),
            "unredacted secret fragment survived the cap: {capped:?}"
        );
        assert!(
            capped.contains("[REDACTED:"),
            "capped prompt carries no redaction marker: {capped:?}"
        );
        assert!(capped.len() <= USER_PROMPT_EXCERPT_MAX_BYTES);
    }

    /// Kimi Code's content-block `prompt` must flatten into the title
    /// exactly like the body excerpt does.
    #[test]
    fn user_prompt_title_flattens_kimi_code_content_blocks() {
        let q = HookQuery {
            event: "user-prompt".into(),
            agent: Some("kimi-code".into()),
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(
            q,
            serde_json::json!({
                "hook_event_name": "UserPromptSubmit",
                "session_id": "session_kimi-1",
                "cwd": "/tmp/proj",
                "prompt": [ { "type": "text", "text": "hello" } ]
            }),
        );
        assert_eq!(env.title_hint.as_deref(), Some("hello"));
        assert_eq!(env.body_excerpt.as_deref(), Some("hello"));
    }

    #[test]
    fn post_tool_excerpt_truncates_without_splitting_utf8() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let output = format!("{}é", "x".repeat(1_999));
        let env = HookEnvelope::from_query_and_body(
            q,
            serde_json::json!({
                "tool_name": "Bash",
                "tool_input": {},
                "result": output,
            }),
        );
        let excerpt = env.body_excerpt.unwrap();
        assert!(excerpt.starts_with("tool_family: non-file\noutcome: unknown\n---\n"));
        let capped = truncate_utf8_bytes(&excerpt, durable_body_cap(HookEvent::PostToolUse));
        assert!(capped.ends_with('…'));
    }

    /// Regression: the native-binary hook command sends the script stem
    /// `user-prompt-submit` as the event token (rendered by `render_shared.rs`,
    /// forwarded verbatim by `ai-memory hook`). The parser must map it to
    /// `UserPrompt`; otherwise native installs (the Windows / posix-native
    /// default) bucket every prompt as `Other` and drop its text.
    #[test]
    fn parses_native_user_prompt_submit_event_token() {
        assert_eq!(
            HookEvent::parse("user-prompt-submit"),
            HookEvent::UserPrompt
        );
        assert_eq!(
            HookEvent::parse("user_prompt_submit"),
            HookEvent::UserPrompt
        );
    }

    /// Claude Code sends `tool_response` as an array of content blocks
    /// (`[{ "type": "text", "text": "…" }]`). The body excerpt must capture
    /// that text instead of falling back to "(no output captured)".
    #[test]
    fn post_tool_excerpt_captures_array_tool_response() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(
            q,
            serde_json::json!({
                "tool_name": "Bash",
                "tool_input": {},
                "tool_response": [{"type": "text", "text": "MARKER_OUTPUT_123"}],
            }),
        );
        let body = env.body_excerpt.expect("post-tool body");
        assert!(
            body.contains("MARKER_OUTPUT_123"),
            "array tool_response text should be captured: {body:?}"
        );
        assert!(
            !body.contains("(no output captured)"),
            "should not fall back when output is present: {body:?}"
        );
    }

    /// An object-shaped `tool_response` is serialized into the body rather than
    /// dropped.
    #[test]
    fn post_tool_excerpt_captures_object_tool_response() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(
            q,
            serde_json::json!({
                "tool_name": "Read",
                "tool_input": {},
                "tool_response": {"stdout": "MARKER_OBJ_456"},
            }),
        );
        let body = env.body_excerpt.expect("post-tool body");
        assert!(
            body.contains("MARKER_OBJ_456"),
            "object tool_response should be serialized into the body: {body:?}"
        );
    }

    /// Cursor runs the Claude Code hooks too, sending its own payload with
    /// Claude's `tool_name`/`tool_input` aliases and the result as a JSON
    /// string in `tool_output`. The payload marks the event as Cursor's, and
    /// Cursor was absent from `closed_tool_agent`, so its tool observations
    /// were stored with no title or body (captured live from Cursor 3.24.9).
    #[test]
    fn cursor_tool_events_keep_their_family_title_and_output() {
        let post = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("claude-code".into()),
                ..Default::default()
            },
            serde_json::json!({
                "conversation_id": "17689370-2e8e-4dfd-bcd0-7e5bb9cd19ed",
                "session_id": "17689370-2e8e-4dfd-bcd0-7e5bb9cd19ed",
                "hook_event_name": "postToolUse",
                "cursor_version": "3.24.9",
                "tool_name": "Shell",
                "tool_input": {"command": "ls", "cwd": "", "timeout": 30000},
                "tool_output": "{\"output\":\"MARKER_CURSOR_OUTPUT\",\"exitCode\":0}",
                "tool_use_id": "9fc522a1-2a0e-4847-9ffc-24256e023574",
                "cwd": "",
                "workspace_roots": [],
            }),
        );
        assert_eq!(post.agent, AgentKind::Cursor);
        assert_eq!(post.title_hint.as_deref(), Some("tool non-file"));
        let body = post.body_excerpt.expect("cursor post-tool body");
        assert!(body.contains("MARKER_CURSOR_OUTPUT"), "{body:?}");

        let pre = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "pre-tool-use".into(),
                agent: Some("claude-code".into()),
                ..Default::default()
            },
            serde_json::json!({
                "session_id": "17689370-2e8e-4dfd-bcd0-7e5bb9cd19ed",
                "hook_event_name": "preToolUse",
                "cursor_version": "3.24.9",
                "tool_name": "Shell",
                "tool_input": {"command": "ls", "cwd": "", "timeout": 30000},
                "tool_use_id": "9fc522a1-2a0e-4847-9ffc-24256e023574",
            }),
        );
        assert_eq!(pre.title_hint.as_deref(), Some("tool non-file"));
        assert!(pre.body_excerpt.is_some_and(|body| !body.is_empty()));
    }

    /// Grok Build CLI posts a `PostToolUse` with Claude Code's snake_case
    /// aliases (`tool_name` / `tool_input` / `tool_use_id`). It was absent from
    /// both `closed_tool_agent` and the `tool_observation_metadata` match, so
    /// every Grok tool observation was stored with an empty body (#931).
    #[test]
    fn grok_post_tool_excerpt_captures_tool_response() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("grok".into()),
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(
            q,
            serde_json::json!({
                "tool_name": "Bash",
                "tool_input": {"command": "ls"},
                "tool_use_id": "call_grok_1",
                "tool_response": {"stdout": "MARKER_GROK_931"},
            }),
        );
        let body = env
            .body_excerpt
            .expect("grok post-tool body should not be empty");
        assert!(
            body.contains("MARKER_GROK_931"),
            "grok tool_response should be serialized into the body: {body:?}"
        );
    }

    /// Copilot CLI's VS-Code-compatible `PostToolUse` nests the output at
    /// `tool_result.text_result_for_llm` (#1040). Without Copilot in
    /// `closed_tool_agent` the observation would be stored with an empty body,
    /// the #931 failure mode Grok had; without the dedicated path the
    /// `result_type` envelope would leak into the excerpt.
    #[test]
    fn copilot_cli_post_tool_excerpt_reads_text_result_for_llm() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("copilot-cli".into()),
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(
            q,
            serde_json::json!({
                "hook_event_name": "PostToolUse",
                "session_id": "copilot-session",
                "cwd": "/repo",
                "tool_name": "bash",
                "tool_input": {"command": "ls"},
                "tool_result": {
                    "result_type": "success",
                    "text_result_for_llm": "MARKER_COPILOT_1040",
                },
            }),
        );
        let body = env
            .body_excerpt
            .expect("copilot-cli post-tool body should not be empty");
        assert!(body.contains("MARKER_COPILOT_1040"), "{body:?}");
        assert!(body.contains("outcome: success"), "{body:?}");
        assert!(!body.contains("result_type"), "{body:?}");
    }

    /// End-to-end: a native-hook user prompt (`event=user-prompt-submit`,
    /// string `prompt`) maps to `UserPrompt` and keeps its body text.
    #[test]
    fn native_user_prompt_submit_keeps_prompt_body() {
        let q = HookQuery {
            event: "user-prompt-submit".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(
            q,
            serde_json::json!({ "session_id": "s1", "prompt": "MARKER_PROMPT_789" }),
        );
        assert_eq!(env.event, HookEvent::UserPrompt);
        assert_eq!(env.body_excerpt.as_deref(), Some("MARKER_PROMPT_789"));
    }

    // Native fields and canonical tool names follow Codex rust-v0.154.0's
    // pre/post-tool-use schemas. `tool_response` accepts any JSON value.
    #[test]
    fn codex_native_tool_pairs_capture_bounded_output_and_stable_metadata() {
        for (tool, input, response, family, text) in [
            (
                "Bash",
                serde_json::json!({"command": "echo private-input"}),
                serde_json::json!("native stdout"),
                "non-file",
                "native stdout",
            ),
            (
                "apply_patch",
                serde_json::json!({"command": "*** Begin Patch\n*** Add File: private-input\n+text\n*** End Patch"}),
                serde_json::json!("Success. Updated the following files:\nA public.txt"),
                "file",
                "public.txt",
            ),
            (
                "Bash",
                serde_json::json!({"command": "private-input"}),
                serde_json::json!({"stdout": "structured stdout", "exit_code": 1}),
                "non-file",
                "structured stdout",
            ),
            (
                "Bash",
                serde_json::json!({"command": "private-input"}),
                serde_json::json!([{"type": "text", "text": "block output"}]),
                "non-file",
                "block output",
            ),
        ] {
            for event in ["PreToolUse", "PostToolUse"] {
                let env = HookEnvelope::from_query_and_body(
                    HookQuery {
                        event: event.into(),
                        agent: Some("codex".into()),
                        ..Default::default()
                    },
                    serde_json::json!({
                        "session_id": "native-codex", "cwd": "/repo", "turn_id": "turn-1",
                        "hook_event_name": event, "permission_mode": "bypassPermissions",
                        "tool_name": tool, "tool_input": input, "tool_response": response,
                        "tool_use_id": "call-native-1",
                    }),
                );
                assert_eq!(env.agent, AgentKind::Codex);
                assert_eq!(env.session_id.as_deref(), Some("native-codex"));
                assert_eq!(env.cwd.as_deref(), Some("/repo"));
                assert_eq!(env.title_hint, Some(format!("tool {family}")));
                let body = env.body_excerpt.unwrap();
                assert!(body.starts_with(&format!(
                    "tool_family: {family}\ntool_call_id: call-native-1"
                )));
                assert!(!body.contains("private-input"));
                if event == "PostToolUse" {
                    assert!(body.contains("outcome: unknown"));
                    assert!(body.contains(text));
                } else {
                    assert!(!body.contains(text));
                    assert!(!body.contains("outcome:"));
                }
            }
        }
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "PostToolUse".into(),
                agent: Some("codex".into()),
                ..Default::default()
            },
            serde_json::json!({"tool_name": "Bash", "tool_use_id": "call-long", "tool_response": {"content": [{"type": "text", "text": "é".repeat(2_000)}]}}),
        );
        // Extraction no longer caps: the sanitizer must see the full text
        // before any byte limit runs (#1114). The cap itself is asserted at
        // the ingest funnel, where it is applied to the scrubbed body.
        let body = env.body_excerpt.unwrap();
        assert!(body.contains('é'));
        assert!(body.len() > TOOL_EXCERPT_MAX_BYTES);
    }

    #[test]
    fn codex_native_unknown_and_malformed_tools_do_not_promote_unproven_content() {
        for raw in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!(42),
            serde_json::json!({"tool_name": 7, "tool_input": {}, "tool_response": "UNPROVEN"}),
            serde_json::json!({"payload": {"tool_name": "Bash", "tool_input": {}, "tool_response": "UNPROVEN"}}),
            serde_json::json!({"tool": "Bash", "args": {}, "output": "UNPROVEN"}),
        ] {
            for event in ["PreToolUse", "PostToolUse"] {
                let env = HookEnvelope::from_query_and_body(
                    HookQuery {
                        event: event.into(),
                        agent: Some("codex".into()),
                        ..Default::default()
                    },
                    raw.clone(),
                );
                assert!(env.title_hint.is_none());
                assert!(env.body_excerpt.is_none());
            }
        }
        for tool in ["mcp__fs__read", "update_plan", "UNPROVEN_TOOL"] {
            let env = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: "PostToolUse".into(),
                    agent: Some("codex".into()),
                    ..Default::default()
                },
                serde_json::json!({"tool_name": tool, "tool_input": {"path": "UNPROVEN"}, "tool_use_id": "call-unknown", "tool_response": {"content": [{"type": "text", "text": "UNPROVEN"}], "structuredContent": {"text": "UNPROVEN"}, "isError": false}}),
            );
            assert_eq!(
                env.body_excerpt.as_deref(),
                Some("tool_family: unknown\ntool_call_id: call-unknown\noutcome: unknown")
            );
        }
        for response in [
            serde_json::json!(null),
            serde_json::json!({}),
            serde_json::json!([]),
        ] {
            let raw = serde_json::json!({"tool_name": "Bash", "tool_use_id": "invalid\nUNPROVEN", "tool_response": response, "output": "UNPROVEN", "error": "UNPROVEN", "payload": {"tool_response": "UNPROVEN"}});
            let pre = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: "PreToolUse".into(),
                    agent: Some("codex".into()),
                    ..Default::default()
                },
                raw.clone(),
            );
            assert!(pre.body_excerpt.is_none(), "missing native input");
            let post = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: "PostToolUse".into(),
                    agent: Some("codex".into()),
                    ..Default::default()
                },
                raw,
            );
            assert_eq!(
                post.body_excerpt.as_deref(),
                Some("tool_family: non-file\noutcome: unknown\n---\n(no output captured)")
            );
        }
    }

    #[test]
    fn closed_tool_summaries_keep_only_safe_metadata_and_cap_total_body() {
        let fixtures = [
            (
                "claude-code",
                serde_json::json!({"tool_name":"Bash","tool_input":{"command":"SENTINEL_COMMAND","path":"SENTINEL_PATH"},"tool_use_id":"claude-1","output":"SENTINEL_OUTPUT"}),
                "claude-1",
                "unknown",
            ),
            (
                "open-code",
                serde_json::json!({"tool":"bash","args":{"command":"SENTINEL_COMMAND"},"callID":"open-1","output":"SENTINEL_OUTPUT"}),
                "open-1",
                "unknown",
            ),
            (
                "pi",
                serde_json::json!({"tool":"bash","args":{"command":"SENTINEL_COMMAND"},"callID":"pi-1","isError":true,"output":"SENTINEL_OUTPUT"}),
                "pi-1",
                "error",
            ),
        ];
        for (agent, raw, id, outcome) in fixtures {
            let pre = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: "pre-tool-use".into(),
                    agent: Some(agent.into()),
                    ..Default::default()
                },
                raw.clone(),
            );
            let pre_body = pre.body_excerpt.expect("pre summary");
            assert!(pre_body.contains(id));
            for sentinel in ["SENTINEL_COMMAND", "SENTINEL_PATH", "SENTINEL_OUTPUT"] {
                assert!(!pre_body.contains(sentinel));
            }
            let post = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: "post-tool-use".into(),
                    agent: Some(agent.into()),
                    ..Default::default()
                },
                raw,
            );
            let post_body = post.body_excerpt.expect("post summary");
            assert!(post_body.contains(&format!("outcome: {outcome}")));
        }

        let long = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("claude-code".into()),
                ..Default::default()
            },
            serde_json::json!({"tool_name":"Bash","tool_input":{},"tool_use_id":"utf8-1","output": "é".repeat(2_000)}),
        );
        let body = long.body_excerpt.unwrap();
        assert!(body.len() > 2_000);
        let capped = truncate_utf8_bytes(&body, durable_body_cap(HookEvent::PostToolUse));
        assert!(capped.len() <= 2_000);
    }

    #[test]
    fn unknown_closed_tool_post_never_includes_output_or_name() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("claude-code".into()),
                ..Default::default()
            },
            serde_json::json!({"tool_name":"ARBITRARY_TOOL_SENTINEL","tool_use_id":"unknown-1","output":"OUTPUT_SENTINEL"}),
        );
        let body = env.body_excerpt.unwrap();
        assert!(body.contains("tool_family: unknown"));
        assert!(body.contains("tool_call_id: unknown-1"));
        assert!(body.contains("outcome: unknown"));
        assert!(!body.contains("ARBITRARY_TOOL_SENTINEL"));
        assert!(!body.contains("OUTPUT_SENTINEL"));
    }

    #[test]
    fn claude_and_opencode_paired_summaries_share_agent_ids() {
        for (agent, pre_raw, post_raw, id) in [
            (
                "claude-code",
                serde_json::json!({"tool_name":"Bash","tool_input":{"command":"PRE_COMMAND_SENTINEL"},"tool_use_id":"claude-pair"}),
                serde_json::json!({"tool_name":"Bash","tool_use_id":"claude-pair","output":"POST_OUTPUT"}),
                "claude-pair",
            ),
            (
                "open-code",
                serde_json::json!({"tool":"bash","args":{"path":"PRE_PATH_SENTINEL"},"callID":"open-pair"}),
                serde_json::json!({"tool":"bash","callID":"open-pair","output":"POST_OUTPUT"}),
                "open-pair",
            ),
        ] {
            let pre = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: "pre-tool-use".into(),
                    agent: Some(agent.into()),
                    ..Default::default()
                },
                pre_raw,
            );
            let post = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: "post-tool-use".into(),
                    agent: Some(agent.into()),
                    ..Default::default()
                },
                post_raw,
            );
            let pre_body = pre.body_excerpt.unwrap();
            let post_body = post.body_excerpt.unwrap();
            assert!(pre_body.contains(id) && post_body.contains(id));
            assert!(post_body.contains("POST_OUTPUT"));
            assert!(!pre_body.contains("SENTINEL"));
        }
    }

    #[test]
    fn antigravity_omits_id_and_unsupported_pre_has_no_body() {
        let antigravity = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("antigravity-cli".into()),
                ..Default::default()
            },
            serde_json::json!({"toolCall":{"name":"run_command","args":{}},"error":"private"}),
        );
        let body = antigravity.body_excerpt.unwrap();
        assert!(body.contains("outcome: error"));
        assert!(!body.contains("tool_call_id"));
        let unsupported = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "pre-tool-use".into(),
                agent: Some("other".into()),
                ..Default::default()
            },
            serde_json::json!({"tool_name":"Bash","tool_input":{"command":"private"}}),
        );
        assert!(unsupported.body_excerpt.is_none());
    }

    #[test]
    fn unsupported_and_stop_tool_payloads_never_render_content() {
        for event in ["pre-tool-use", "post-tool-use"] {
            let env = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: event.into(),
                    agent: Some("other".into()),
                    ..Default::default()
                },
                serde_json::json!({"tool":"SENTINEL_TOOL","args":{"command":"SENTINEL_COMMAND","path":"SENTINEL_PATH"},"output":"SENTINEL_OUTPUT"}),
            );
            assert!(env.title_hint.is_none());
            assert!(env.body_excerpt.is_none());
        }
        let stop = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "stop".into(),
                agent: Some("claude-code".into()),
                ..Default::default()
            },
            serde_json::json!({"last_assistant_message":"SENTINEL_ASSISTANT"}),
        );
        assert!(stop.body_excerpt.is_none());
    }

    #[test]
    fn antigravity_outcome_and_call_id_boundaries_are_closed() {
        let absent = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("antigravity-cli".into()),
                ..Default::default()
            },
            serde_json::json!({"toolCall":{"name":"run_command","args":{}}}),
        );
        assert!(absent.body_excerpt.unwrap().contains("outcome: unknown"));
        for (error, outcome) in [
            (serde_json::json!(null), "unknown"),
            (serde_json::json!(""), "unknown"),
            (serde_json::json!("failed"), "error"),
        ] {
            let env = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: "post-tool-use".into(),
                    agent: Some("antigravity-cli".into()),
                    ..Default::default()
                },
                serde_json::json!({"toolCall":{"name":"run_command","args":{}},"error":error}),
            );
            assert!(
                env.body_excerpt
                    .unwrap()
                    .contains(&format!("outcome: {outcome}"))
            );
        }
        let id = "a".repeat(129);
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "pre-tool-use".into(),
                agent: Some("claude-code".into()),
                ..Default::default()
            },
            serde_json::json!({"tool_name":"Bash","tool_input":{},"tool_use_id":id}),
        );
        assert!(!env.body_excerpt.unwrap().contains("tool_call_id"));
    }

    #[test]
    fn antigravity_native_file_and_search_tools_render_captured_content() {
        for (tool, args, family) in [
            (
                "view_file",
                serde_json::json!({"TargetFile": "src/main.rs"}),
                "file",
            ),
            (
                "replace_file_content",
                serde_json::json!({"TargetFile": "src/main.rs"}),
                "file",
            ),
            (
                "list_dir",
                serde_json::json!({"DirectoryPath": "src"}),
                "search-list",
            ),
            (
                "grep_search",
                serde_json::json!({"SearchPath": "src"}),
                "search-list",
            ),
        ] {
            let env = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: "post-tool-use".into(),
                    agent: Some("antigravity-cli".into()),
                    ..Default::default()
                },
                serde_json::json!({
                    "toolCall": {"name": tool, "args": args},
                    "tool_response": "SENTINEL_CONTENT",
                }),
            );
            let body = env.body_excerpt.unwrap();
            assert!(
                body.contains(&format!("tool_family: {family}")),
                "tool: {tool}, body: {body}"
            );
            assert!(
                body.contains("SENTINEL_CONTENT"),
                "tool: {tool}, body: {body}"
            );
        }
    }

    #[test]
    fn antigravity_edit_tools_capture_real_written_content() {
        // Fixture shapes captured from a live antigravity-cli session. The hook
        // never sends a top-level result; the written content lives in args.
        let write_to_file = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("antigravity-cli".into()),
                ..Default::default()
            },
            serde_json::json!({
                "toolCall": {
                    "name": "write_to_file",
                    "args": {
                        "CodeContent": "# Scratch Test File 09\n\nLine 1: first line\n",
                        "Description": "Temporary scratch file",
                        "Overwrite": true,
                        "TargetFile": "/repo/scratch-test-09.md"
                    }
                }
            }),
        );
        let body = write_to_file.body_excerpt.unwrap();
        assert!(body.contains("tool_family: file"));
        assert!(body.contains("Scratch Test File 09"));

        let replace_file_content = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("antigravity-cli".into()),
                ..Default::default()
            },
            serde_json::json!({
                "toolCall": {
                    "name": "replace_file_content",
                    "args": {
                        "TargetFile": "/repo/1-visao.md",
                        "TargetContent": "old line",
                        "ReplacementContent": "new line REPLACED_SENTINEL",
                        "Instruction": "Add debug test comment"
                    }
                }
            }),
        );
        let body = replace_file_content.body_excerpt.unwrap();
        assert!(body.contains("tool_family: file"));
        assert!(body.contains("REPLACED_SENTINEL"));

        let multi_replace = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("antigravity-cli".into()),
                ..Default::default()
            },
            serde_json::json!({
                "toolCall": {
                    "name": "multi_replace_file_content",
                    "args": {
                        "TargetFile": "/repo/scratch-test-09.md",
                        "Instruction": "Edit Line 1 and Line 4",
                        "ReplacementChunks": [
                            {
                                "TargetContent": "Line 1: first line",
                                "ReplacementContent": "Line 1: first line edited",
                                "StartLine": 3,
                                "EndLine": 3,
                                "AllowMultiple": false
                            },
                            {
                                "TargetContent": "Line 4: fourth line",
                                "ReplacementContent": "Line 4: fourth line edited",
                                "StartLine": 6,
                                "EndLine": 6,
                                "AllowMultiple": false
                            }
                        ]
                    }
                }
            }),
        );
        let body = multi_replace.body_excerpt.unwrap();
        assert!(body.contains("tool_family: file"));
        assert!(body.contains("Line 1: first line edited"));
        assert!(body.contains("Line 4: fourth line edited"));
    }

    #[test]
    fn antigravity_failed_edit_prefers_error_over_attempted_content() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("antigravity-cli".into()),
                ..Default::default()
            },
            serde_json::json!({
                "error": "EDIT_FAILED_SENTINEL",
                "toolCall": {
                    "name": "replace_file_content",
                    "args": {
                        "TargetFile": "/repo/src/main.rs",
                        "ReplacementContent": "CONTENT_WAS_NOT_WRITTEN"
                    }
                }
            }),
        );
        let body = env.body_excerpt.unwrap();
        assert!(body.contains("outcome: error"));
        assert!(body.contains("EDIT_FAILED_SENTINEL"));
        assert!(!body.contains("CONTENT_WAS_NOT_WRITTEN"));
    }

    #[test]
    fn antigravity_generic_tools_and_unrelated_events_fail_closed() {
        for tool in ["read_url_content", "read_resource", "call_mcp_tool"] {
            let env = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: "post-tool-use".into(),
                    agent: Some("antigravity-cli".into()),
                    ..Default::default()
                },
                serde_json::json!({
                    "toolCall": {
                        "name": tool,
                        "args": {"message": "NESTED_ARGUMENT_SENTINEL"}
                    },
                    "tool_response": "UNPROVEN_OUTPUT_SENTINEL"
                }),
            );
            assert_eq!(
                env.body_excerpt.as_deref(),
                Some("tool_family: unknown\noutcome: unknown")
            );
        }

        let notification = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "notification".into(),
                agent: Some("antigravity-cli".into()),
                ..Default::default()
            },
            serde_json::json!({
                "toolCall": {
                    "args": {"message": "NESTED_ARGUMENT_SENTINEL"}
                }
            }),
        );
        assert!(notification.body_excerpt.is_none());
    }

    #[test]
    fn antigravity_post_tool_use_extracts_output_txt_content() {
        let dir = tempfile::tempdir().unwrap();
        let step_dir = dir
            .path()
            .join(".system_generated")
            .join("steps")
            .join("42");
        std::fs::create_dir_all(&step_dir).unwrap();
        std::fs::write(step_dir.join("output.txt"), "cargo test: 10 passed\n").unwrap();

        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("antigravity-cli".into()),
                ..Default::default()
            },
            serde_json::json!({
                "conversationId": "conv-1",
                "toolCall": {
                    "name": "run_command",
                    "args": {"CommandLine": "cargo test"}
                },
                "stepIdx": 42,
                "artifactDirectoryPath": dir.path().to_str().unwrap()
            }),
        );
        let body = env.body_excerpt.expect("body excerpt should be present");
        assert!(body.contains("tool_family: non-file"));
        assert!(body.contains("cargo test: 10 passed"));
    }

    #[test]
    fn antigravity_post_tool_use_preserves_edit_tool_code_content() {
        let dir = tempfile::tempdir().unwrap();
        let step_dir = dir.path().join(".system_generated").join("steps").join("1");
        std::fs::create_dir_all(&step_dir).unwrap();
        std::fs::write(step_dir.join("output.txt"), "Created file test.rs\n").unwrap();

        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("antigravity-cli".into()),
                ..Default::default()
            },
            serde_json::json!({
                "conversationId": "conv-1",
                "toolCall": {
                    "name": "write_to_file",
                    "args": {
                        "TargetFile": "/workspace/test.rs",
                        "CodeContent": "fn preserved_code() {}"
                    }
                },
                "stepIdx": 1,
                "artifactDirectoryPath": dir.path().to_str().unwrap()
            }),
        );
        let body = env.body_excerpt.expect("body excerpt should be present");
        assert!(body.contains("fn preserved_code() {}"));
        assert!(!body.contains("Created file test.rs"));
    }

    #[test]
    fn pi_and_omp_post_outcomes_and_stable_id_are_rendered() {
        for agent in ["pi", "omp"] {
            for (is_error, outcome) in [
                (Some(false), "success"),
                (Some(true), "error"),
                (None, "unknown"),
            ] {
                let mut raw = serde_json::json!({"tool":"bash","args":{},"callID":"pi-stable-190","output":"result"});
                if let Some(is_error) = is_error {
                    raw["isError"] = serde_json::json!(is_error);
                }
                let pre = HookEnvelope::from_query_and_body(
                    HookQuery {
                        event: "pre-tool-use".into(),
                        agent: Some(agent.into()),
                        ..Default::default()
                    },
                    raw.clone(),
                );
                let post = HookEnvelope::from_query_and_body(
                    HookQuery {
                        event: "post-tool-use".into(),
                        agent: Some(agent.into()),
                        ..Default::default()
                    },
                    raw,
                );
                assert!(
                    pre.body_excerpt
                        .unwrap()
                        .contains("tool_call_id: pi-stable-190")
                );
                let body = post.body_excerpt.unwrap();
                assert!(body.contains("tool_call_id: pi-stable-190"));
                assert!(
                    body.contains(&format!("outcome: {outcome}")),
                    "{agent}: {body}"
                );
            }
        }
    }

    #[test]
    fn omp_tool_events_keep_family_call_id_and_output_but_not_arguments() {
        // `build_pi_extension` in the CLI derives the Pi extension from the OMP
        // one, so OMP posts this exact tool payload shape.
        let pre = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "pre-tool-use".into(),
                agent: Some("omp".into()),
                ..Default::default()
            },
            serde_json::json!({"tool":"bash","callID":"omp-1","args":{"command":"SENTINEL_COMMAND"}}),
        );
        assert_eq!(pre.title_hint.as_deref(), Some("tool non-file"));
        assert_eq!(
            pre.body_excerpt.as_deref(),
            Some("tool_family: non-file\ntool_call_id: omp-1")
        );

        let post = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("omp".into()),
                ..Default::default()
            },
            serde_json::json!({"tool":"bash","callID":"omp-1","args":{"command":"SENTINEL_COMMAND"},"output":"tests passed","details":{"diff":"SENTINEL_DETAILS"}}),
        );
        assert_eq!(post.title_hint.as_deref(), Some("tool non-file"));
        assert_eq!(
            post.body_excerpt.as_deref(),
            Some("tool_family: non-file\ntool_call_id: omp-1\noutcome: unknown\n---\ntests passed")
        );
    }
}
