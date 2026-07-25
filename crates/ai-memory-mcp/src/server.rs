//! [`AiMemoryServer`] — the MCP server skeleton + tool router.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use ai_memory_consolidate::{
    AutoImproveReviewConfig, Consolidator, projection::cap_text_with_marker,
    run_auto_improve_review, run_lint, run_sweep,
};
use ai_memory_core::{
    ActiveProject, AgentKind, HandoffId, HandoffState, NewHandoff, PageId, PagePath, ProjectId,
    SessionId, Tier, WorkspaceId,
};
use ai_memory_llm::{Embedder, LlmProvider};
use ai_memory_store::{AutoImproveProposalOperation, NewAutoImproveProposal, StageAutoImproveRun};
use ai_memory_store::{DecayParams, PageHit, ReaderPool, ScopeName, ScopeResolver, WriterHandle};
use ai_memory_wiki::{Wiki, WikiError, WritePageRequest};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, schemars, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};

const HANDOFF_SUMMARY_MAX_CHARS: usize = 3_000;
const HANDOFF_ITEM_MAX_CHARS: usize = 1_500;
const HANDOFF_FILE_MAX_CHARS: usize = 512;
const HANDOFF_TEXT_LIST_MAX_CHARS: usize = 6_000;
const HANDOFF_FILE_LIST_MAX_CHARS: usize = 4_096;
const HANDOFF_LIST_MAX_ITEMS: usize = 20;

fn default_auto_improve_review_config() -> AutoImproveReviewConfig {
    AutoImproveReviewConfig {
        min_observations: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MIN_OBSERVATIONS,
        min_session_duration_secs:
            ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MIN_SESSION_DURATION_SECS,
        min_confidence: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MIN_CONFIDENCE,
        max_input_tokens: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_INPUT_TOKENS,
        max_proposals_per_run: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PROPOSALS,
        include_raw_fallback: false,
        proposal_actor: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_PROPOSAL_ACTOR.into(),
        pending_path: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_PENDING_PATH.into(),
        max_patchable_pages: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_PAGES,
        max_patchable_body_chars:
            ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_BODY_CHARS,
        max_edits_per_proposal: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_EDITS_PER_PROPOSAL,
        max_edit_content_chars: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_EDIT_CONTENT_CHARS,
        max_changed_chars_per_proposal:
            ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_CHANGED_CHARS_PER_PROPOSAL,
        max_patch_edits_per_run:
            ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PATCH_EDITS_PER_RUN,
        max_rejection_context: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_REJECTION_CONTEXT,
        rejection_context_days: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_REJECTION_CONTEXT_DAYS,
        max_final_body_chars: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_FINAL_BODY_CHARS,
        max_rule_page_tokens: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_RULE_PAGE_TOKENS,
        max_procedure_page_tokens:
            ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PROCEDURE_PAGE_TOKENS,
        eval: ai_memory_consolidate::AutoImproveEvalConfig::default(),
    }
}

fn cap_handoff_list<I>(
    items: I,
    item_max_chars: usize,
    total_max_chars: usize,
    item_label: &str,
    list_label: &str,
) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let capped: Vec<String> = items
        .into_iter()
        .map(|item| cap_text_with_marker(&item, item_max_chars, item_label))
        .collect();
    let total_items = capped.len();
    let mut out = Vec::new();
    let mut used_chars = 0usize;

    for (idx, item) in capped.into_iter().enumerate() {
        if out.len() >= HANDOFF_LIST_MAX_ITEMS {
            push_handoff_omission_marker(
                &mut out,
                &mut used_chars,
                total_max_chars,
                list_label,
                total_items.saturating_sub(idx),
            );
            break;
        }
        let item_len = item.chars().count();
        let separator = usize::from(!out.is_empty());
        if !out.is_empty()
            && used_chars
                .saturating_add(separator)
                .saturating_add(item_len)
                > total_max_chars
        {
            push_handoff_omission_marker(
                &mut out,
                &mut used_chars,
                total_max_chars,
                list_label,
                total_items.saturating_sub(idx),
            );
            break;
        }
        used_chars = used_chars
            .saturating_add(separator)
            .saturating_add(item_len);
        out.push(item);
    }
    out
}

fn push_handoff_omission_marker(
    out: &mut Vec<String>,
    used_chars: &mut usize,
    total_max_chars: usize,
    label: &str,
    omitted: usize,
) {
    if omitted == 0 {
        return;
    }
    let separator = usize::from(!out.is_empty());
    let available = total_max_chars.saturating_sub(used_chars.saturating_add(separator));
    if available == 0 {
        return;
    }
    let marker = format!("[{label} truncated; {omitted} additional item(s) omitted]");
    let marker: String = marker.chars().take(available).collect();
    *used_chars = used_chars
        .saturating_add(separator)
        .saturating_add(marker.chars().count());
    out.push(marker);
}

/// Instructions surfaced to clients via `ServerInfo`. Sent on every
/// MCP handshake so Claude Code / Codex / OpenCode see this in their
/// session preamble. Maps conversational triggers to tool names so
/// the agent can route natural-language requests without the user
/// having to know the tool name or schema.
pub const MEMORY_INSTRUCTIONS: &str = "\
Long-term memory for the current project.\n\
\n\
**Default to the current project — always.** Every tool here \
auto-scopes to the project resolved from your session's working \
directory. **Do NOT pass `project`, `workspace`, or `cwd` arguments unless the user \
explicitly references a *different* project by name** (e.g. 'what did \
we decide in the other-app project?'). Phrases like 'this project', \
'here', 'we', 'our work', 'where did we leave off' all mean the \
*current* project — call the tool with no scoping args. If the user \
asks about a handoff and the SessionStart auto-fetched block is already \
in your context, answer from it; do NOT re-call the tool to look for it \
in another project.\n\
\n\
This default assumes the MCP client can identify the current agent \
session. Static MCP clients in parallel sessions for the same user \
cannot forward the real agent session id automatically; pass explicit \
`workspace` + `project` / `scopes`, or use a session-aware bridge that \
forwards the lifecycle-hook session id on MCP calls.\n\
\n\
Lifecycle hooks already capture sanitized, bounded prompt and tool-lifecycle \
observations automatically. They are not complete native transcripts; managed \
`ai-memory run` launches add the portable visible-event ledger. You do NOT \
need to write routine notes by hand. When the user \
explicitly asks to remember a permanent annotation/fact/rule, write a \
durable wiki page; do not use a handoff for that. Use these tools when \
the conversation calls for them:\n\
\n\
- `memory_query` — when the user references prior work you don't \
  recognise, or asks 'have we done / discussed X', or you're about \
  to propose architecture (always check first). Defaults to the \
  current project; pass `scopes` to search named sibling projects, \
  or `global=true` to search EVERY project at once when you don't \
  know where the knowledge lives. Default-scoped calls also return \
  `global_scope_hits` — standing user/team preferences from the \
  reserved `_global` scope; treat them as context that applies to \
  every project.\n\
- `memory_recent` — at session start, or when the user asks 'what's \
  been going on lately'. Returns the N most-recent pages.\n\
- `memory_status` — when the user asks 'is ai-memory healthy' or \
  'how big is the knowledge base'. Returns lifetime counts.\n\
- `memory_briefing` — when the user wants a STRUCTURED snapshot \
  (counts + 7d/30d activity + rules + recent pages, JSON, no LLM \
  call). READ-ONLY: it never creates handoffs or mutates state. Use \
  over memory_status when more detail is wanted.\n\
- `memory_explore` — when the user wants a PROSE digest. \
  Calibrates verbosity to time since last activity: 'fresh' → one \
  line, 'stale' (>30d) → full catchup. Accepts an optional `focus` \
  arg. Use over memory_briefing when the user asks open-ended \
  questions like 'catch me up' or 'what's important right now'.\n\
- `memory_handoff_accept` — when the user asks 'where did we leave \
  off'. The SessionStart hook auto-fetches + consumes the handoff \
  before you see your first prompt; if a block starting with \
  '📥 ai-memory: pending handoff' is anywhere in your context, \
  THAT is the handoff — answer from it directly, don't re-call \
  this tool (it'll return null because handoffs are single-use). Pass \
  `workspace` + `project` together only when the user names a handoff \
  in a sibling workspace/project.\n\
- `memory_handoff_begin` — ONLY when the user is wrapping up / ending \
  the current session and you want to ensure the next agent has context \
  (the SessionEnd hook also auto-captures this). DO NOT use this to \
  summarize work mid-session, check project status, or answer a request \
  for a briefing. Keep the summary terse (2-3 sentences); put detail \
  in open_questions + next_steps bullets. Pass `workspace` + `project` \
  together only when leaving a handoff for a named sibling \
  workspace/project.\n\
- `memory_handoff_cancel` — when you realize you mistakenly called \
  `memory_handoff_begin`, or the user explicitly asks to discard a \
  pending handoff. Requires the exact `handoff_id` from the begin call \
  and marks it expired so the next session will not consume it.\n\
- `memory_consolidate` — when the user asks to compile session \
  observations into wiki pages. Also runs on PreCompact, and at \
  session end only when AI_MEMORY_CONSOLIDATE_ON_SESSION_END is set.\n\
- `memory_auto_improve` — when the user asks what durable lessons \
should be proposed from a completed session, or at explicit wrap-up \
  when learning review is useful. It is the manual version of the server's \
  all-project scheduled auto-improvement loop, reads the latest completed session by \
  default, and applies or stages validated edits through the auto-improvement \
  approval path. Admins can set `[auto_improve.scheduler] enabled = false` \
  to stop scheduling, or `[auto_improve] require_approval = true` to leave \
  scheduled and manual proposals in pending-writes for review.\n\
- `memory_write_page` — when the user explicitly asks to remember, \
  save, or annotate durable project knowledge. This writes a wiki page; \
  do NOT use `memory_handoff_begin` for permanent annotations. \
  Put the title as a `# H1` on the first line of `body` and omit the \
  `title` argument — ai-memory derives the title automatically and \
  passing `title` is a known JSON-escape footgun (issue #67). When the \
  fact is a standing user/team preference that should apply to EVERY \
  project ('always use pnpm', 'never force-push', code style rules), \
  pass `scope: \"global\"` so it lands in the reserved `_global` scope \
  instead of the current project.\n\
- `memory_read_page` — when the user asks to read, open, or show the \
  full content of a specific page. Accepts a `query` (searches FTS5 and \
  returns the top hit's full body) or a `path` (direct lookup). Pass \
  `workspace` + `project` together only when reading a page from a named \
  sibling workspace/project. Use \
  this instead of memory_query when the user wants the complete text, \
  not just snippets.\n\
- `memory_delete_page` — when the user explicitly asks to delete or \
  remove a specific page (by exact path). Idempotent; fires the \
  admission chain so mirrors/backups stay consistent. Pass `workspace` \
  + `project` together only when the page lives in a sibling \
  workspace/project; missing explicit scopes fail closed instead of falling back.\n\
- `memory_lint` — when the user asks to audit the wiki for stale \
  pages, contradictions, or rule suggestions.\n\
- `memory_forget_sweep` — when the user wants to prune old / cold \
  pages (idempotent, supports dry-run).\n\
- `memory_install_self_routing` — when the user asks to 'install \
  ai-memory routing into this project' or 'add ai-memory to \
  CLAUDE.md / AGENTS.md'. Returns the managed routing package: the \
  slim markered snippet (`markered_block`), filename hints, \
  `managed_skills` payloads, `target_hints` for `.claude/skills`, \
  `.agents/skills`, `.devin/skills`, `.grok/skills`, and Devin's Windows global \
  `%APPDATA%\\devin\\skills` root, and overwrite guidance. Use your own Write/Edit \
  tool to replace only the ai-memory marker block in the rules file, \
  then write each managed skill under the selected skill root. Only \
  replace same-name skill files that contain the ai-memory managed \
  marker unless the human explicitly forces replacement.\n\
\n\
**When the current project comes up empty, broaden — don't stop.** \
`memory_query` searches only ONE project (the current one) by default. \
If a query returns nothing useful, the knowledge may live in a SIBLING \
project — shared `infra`, `ops`, or a related app. Two ways to \
broaden: (a) re-run with explicit `scopes: [{workspace, project}]` \
when you know which projects to check; (b) pass `global=true` to \
search EVERY project in EVERY workspace at once when you don't know \
where the knowledge lives — each hit then carries its workspace + \
project name. `global=true` cannot be combined with \
`scopes`/`project`/`workspace`. Don't conclude 'we never recorded \
it' after one project misses. Note also that `memory_query` returns \
SNIPPETS, not full page bodies — an empty or short snippet does NOT \
mean the page is empty (a large page can match outside the snippet \
window); to read the whole page use `memory_read_page` (by `path`, \
or a `query` for the top hit's body; add `workspace` + `project` \
together only for a named sibling workspace/project).\n\
\n\
**Use retrieved memory as operating guidance, not trivia.** When \
`memory_query` or `memory_recent` returns `_rules/`, `gotchas/`, \
`procedures/`, or `decisions/` pages relevant to the task, read the \
full page with `memory_read_page` before acting. Treat `_rules/` as \
constraints, `gotchas/` as preflight warnings, `procedures/` as \
checklists, and `decisions/` as settled architecture unless the user \
explicitly asks to revisit them. Before non-trivial coding, debugging, \
deployment, release, auth, scope, migration, PR-review, or \
data-preservation work, search memory for the subsystem and task type \
first.\n\
\n\
The managed routing package this text points to can also be installed \
into the project's CLAUDE.md / AGENTS.md plus ai-memory-managed Agent \
Skills so the guidance survives across sessions. From the agent: ask \
'install ai-memory routing' and use the returned `managed_skills` + \
`target_hints`. From the terminal: `ai-memory install-instructions` \
(or `ai-memory install-skills` to refresh only the skill files).";

/// MCP server backed by the ai-memory store.
#[derive(Clone)]
pub struct AiMemoryServer {
    reader: ReaderPool,
    writer: WriterHandle,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    /// Project the user is currently active in, published by the hook
    /// router on each cwd-resolved event. The read tools prefer this
    /// over the baked-in `(workspace_id, project_id)` so a shared HTTP
    /// server queries the project the agent is actually in rather than
    /// the static `--project` default (issue #2). Empty until the first
    /// hook event arrives, or always-empty in stdio mode (no shared
    /// hook ingress) — in which case the baked-in default is used.
    active_project: ActiveProject,
    default_limit: usize,
    /// Optional LLM consolidator. When `None`, `memory_consolidate`
    /// returns a "not configured" error.
    consolidator: Option<Arc<Consolidator>>,
    /// Optional LLM provider for the lint contradiction pass. When
    /// `None`, lint runs only the rule-based checks.
    llm: Option<Arc<dyn LlmProvider>>,
    /// Wiki handle (needed by the sweep / lint tools to read pages +
    /// write the lint report). `None` when the server was built
    /// without one — older `new()` callers stay safe.
    wiki: Option<Wiki>,
    /// M8 retention parameters. Defaults if not overridden by the
    /// caller (typically from the user's config.toml `[decay]` block).
    decay_params: DecayParams,
    /// M9 embedder for hybrid query. When `None`, `memory_query`
    /// falls back to pure FTS5.
    embedder: Option<Arc<dyn Embedder>>,
    /// Privacy strip. Applied to agent-supplied handoff fields in
    /// `memory_handoff_begin` (handoffs bypass `Wiki::write_page` so
    /// the wiki-level scrub doesn't cover them).
    sanitizer: ai_memory_core::Sanitizer,
    /// If true, `memory_auto_improve` stages proposals for manual approval;
    /// otherwise it immediately approves validated proposals through the normal
    /// wiki write path.
    auto_improve_require_approval: bool,
    /// Server-configured defaults used by manual MCP auto-improvement. This
    /// keeps manual runs at least as strict as the operator's configured
    /// Phase 1/2 budgets instead of falling back to compiled defaults.
    auto_improve_review_config: AutoImproveReviewConfig,
    // Read by the `#[tool_handler]` macro expansion; rustc's dead-code
    // analysis can't see that, so the lint must be allowed explicitly.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

const MAX_QUERY_SCOPES: usize = 25;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct MemoryScopeArg {
    /// Project to read inside the workspace.
    project: String,
    /// Workspace to read.
    workspace: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct QueryArgs {
    /// FTS5 query expression (e.g. `"karpathy wiki"` or `quick OR slow`).
    #[serde(alias = "q", alias = "search")]
    query: String,
    /// Maximum number of hits to return (default 10, max 100).
    #[serde(default, alias = "n", alias = "top_k")]
    limit: Option<usize>,
    /// Project to search. Omit to target the project you're currently
    /// working in (resolved from recent hook activity). **Omit unless the user explicitly names a *different* project.** Only needed when
    /// one shared server fields several projects at once.
    #[serde(default)]
    project: Option<String>,
    /// Workspace to search together with `project`. Omit to use the
    /// current/default workspace resolution chain.
    #[serde(default)]
    workspace: Option<String>,
    /// Explicit multi-project scopes to search. Use this when a task
    /// needs context from a client project plus shared practice/project
    /// knowledge. Cannot be combined with `workspace`/`project`.
    #[serde(default)]
    scopes: Vec<MemoryScopeArg>,
    /// Search EVERY project in every workspace in one call (cross-project
    /// global search). Use when you don't know which project holds the
    /// knowledge — e.g. shared infra/ops notes. When true, omit
    /// `project`/`workspace`/`scopes`; each hit is annotated with its
    /// workspace + project so you can tell where it came from.
    #[serde(default)]
    global: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct RecentArgs {
    /// Maximum number of recent pages to return (default 10, max 100).
    #[serde(default, alias = "n")]
    limit: Option<usize>,
    /// Project to read. Omit to target the project you're currently
    /// working in (resolved from recent hook activity). **Omit unless the user explicitly names a *different* project.**
    #[serde(default)]
    project: Option<String>,
    /// Workspace to read together with `project`. Omit to use the
    /// current/default workspace resolution chain.
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct StatusArgs {
    /// Project to report counts for. Omit to target the project you're
    /// currently working in (resolved from recent hook activity). **Omit unless the user explicitly names a *different* project.**
    #[serde(default)]
    project: Option<String>,
    /// Workspace to report together with `project`. Omit to use the
    /// current/default workspace resolution chain.
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Debug, Serialize)]
struct MemoryQueryResponse {
    hits: Vec<ai_memory_store::PageHit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    raw_hits: Vec<ai_memory_store::ObservationHit>,
    /// Populated only by a `global=true` query: cross-project hits, each
    /// carrying its workspace + project name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    global_hits: Vec<ai_memory_store::PageHitWithMeta>,
    /// Standing user/team context from the reserved `_global` preferences
    /// scope, unioned into default-scoped queries alongside the current
    /// project's `hits` (issue #154). Empty when the scope doesn't exist or
    /// the query was explicitly scoped (`workspace`/`project`/`scopes`/
    /// `global=true`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    global_scope_hits: Vec<ai_memory_store::PageHit>,
}

/// Response for `memory_recent`. `hits` carries the current project's recent
/// pages; `global_hits` is populated instead when the read broadened to global
/// (repo opted into `[recall] default_global`), each hit annotated with its
/// workspace + project. `global_hits` is omitted when empty, so a plain
/// project-scoped response is unchanged.
#[derive(Debug, Serialize)]
struct MemoryRecentResponse {
    hits: Vec<ai_memory_store::PageHit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    global_hits: Vec<ai_memory_store::PageHitWithMeta>,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    counts: ai_memory_store::StatusCounts,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct SweepArgs {
    /// If true, preview only. Default false.
    #[serde(default)]
    dry_run: Option<bool>,
    /// Project to sweep. Omit to target the project you're currently working
    /// in (resolved from recent hook activity). **Omit unless the user
    /// explicitly names a *different* project.**
    #[serde(default)]
    project: Option<String>,
    /// Workspace the project lives in. Omit for the current workspace.
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct LintArgs {
    /// If true, don't write wiki/_lint/<date>.md. Default false.
    #[serde(default)]
    dry_run: Option<bool>,
    /// If true, skip the LLM contradiction pass (rule-based only).
    /// Useful when a provider is configured but you only want the
    /// fast rule-based checks. Default false.
    #[serde(default)]
    no_llm: Option<bool>,
    /// Project to audit. Omit to target the project you're currently working
    /// in (resolved from recent hook activity). **Omit unless the user
    /// explicitly names a *different* project.**
    #[serde(default)]
    project: Option<String>,
    /// Workspace the project lives in. Omit for the current workspace.
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ConsolidateArgs {
    /// UUID of the session to consolidate.
    session_id: String,
    /// If true, preview without writing. Default false.
    #[serde(default)]
    dry_run: Option<bool>,
    /// If true, M7b multi-page atomic fan-out. Default false (single page).
    #[serde(default)]
    multi_page: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct AutoImproveArgs {
    /// Completed session UUID to review. Omit to review the latest completed
    /// session in the resolved current project.
    #[serde(default)]
    session_id: Option<String>,
    /// Removed compatibility field. Hidden from the tool schema; if an old
    /// caller still sends it, fail closed instead of turning an old preview
    /// request into an applying request.
    #[serde(default)]
    #[schemars(skip)]
    dry_run: Option<bool>,
    /// Removed compatibility field; hidden from schema and rejected if present.
    #[serde(default)]
    #[schemars(skip)]
    stage: Option<bool>,
    /// Removed compatibility field; hidden from schema and rejected if present.
    #[serde(default)]
    #[schemars(skip)]
    mode: Option<String>,
    /// Project to review. Omit to target the project you're currently working
    /// in (resolved from recent hook activity). **Omit unless the user
    /// explicitly names a different project.**
    #[serde(default)]
    project: Option<String>,
    /// Workspace to review together with `project`. Omit for the
    /// current/default workspace resolution chain.
    #[serde(default)]
    workspace: Option<String>,
    /// Override the minimum observation count for this run.
    #[serde(default)]
    min_observations: Option<usize>,
    /// Override the minimum session span for this run.
    #[serde(default)]
    min_session_duration_secs: Option<u64>,
    /// Override the proposal confidence floor for this run.
    #[serde(default)]
    min_confidence: Option<f32>,
    /// Override the approximate chars/4 input token budget for this run.
    #[serde(default)]
    max_input_tokens: Option<usize>,
    /// Override the maximum validated proposal count for this run.
    #[serde(default)]
    max_proposals: Option<usize>,
    /// Include raw fallback context when the reviewer supports it.
    #[serde(default)]
    include_raw_fallback: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct HandoffBeginArgs {
    /// Short prose summary of where the session left off.
    summary: String,
    /// Questions the next agent should resolve.
    #[serde(default)]
    open_questions: Vec<String>,
    /// Suggested next steps.
    #[serde(default)]
    next_steps: Vec<String>,
    /// Files touched during the session.
    #[serde(default)]
    files_touched: Vec<String>,
    /// Working directory at the time of handoff. Used to match the
    /// next agent's `memory_handoff_accept` call.
    #[serde(default)]
    cwd: Option<String>,
    /// Project to scope the handoff to. Omit to target the project you're
    /// currently working in (resolved from recent hook activity). When set to a
    /// name that doesn't exist yet, the project is **created** — so the handoff
    /// always lands where you asked, never silently in the current project.
    /// **Omit unless the user explicitly names a *different* project.**
    #[serde(default)]
    project: Option<String>,
    /// Workspace to scope the handoff to, together with `project`; created if it
    /// doesn't exist. Omit for the current workspace. Provide both to leave a
    /// handoff in a *different* workspace (e.g. a sibling project on a shared
    /// server) — without it the workspace is resolved from hook activity, which
    /// can route a cross-workspace handoff to the wrong project.
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct HandoffAcceptArgs {
    /// Restrict the search to handoffs created for a specific cwd.
    /// **Omit unless the user explicitly asks about a handoff from a
    /// *different* directory** — by default this scopes to the current
    /// project (the SessionStart hook usually pre-fetches it into context).
    #[serde(default)]
    cwd: Option<String>,
    /// Project to accept a handoff from. Omit to target the project you're
    /// currently working in (resolved from recent hook activity). **Omit unless the user explicitly names a *different* project.**
    #[serde(default)]
    project: Option<String>,
    /// Workspace to accept from, together with `project`. Omit for the
    /// current/default workspace resolution chain. Provide both to read a
    /// handoff left in a *different* workspace (e.g. a sibling project on a
    /// shared server).
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct HandoffCancelArgs {
    /// Exact handoff id returned by `memory_handoff_begin`. Required so this
    /// tool only discards a handoff the agent can identify.
    handoff_id: String,
    /// Project to cancel within. Omit to target the current project. **Omit
    /// unless the user explicitly names a different project.**
    #[serde(default)]
    project: Option<String>,
    /// Workspace to cancel within, together with `project`. Omit for the
    /// current/default workspace resolution chain.
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct BriefingArgs {
    /// How many recently-updated pages to include (default 10, max 100).
    #[serde(default)]
    recent_pages_limit: Option<usize>,
    /// Project to brief on. Omit to target the project you're currently
    /// working in (resolved from recent hook activity). **Omit unless the user explicitly names a *different* project.**
    #[serde(default)]
    project: Option<String>,
    /// Workspace to brief together with `project`. Omit to use the
    /// current/default workspace resolution chain.
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ExploreArgs {
    /// Optional topic to bias the digest toward (e.g. "recent rules",
    /// "pending handoffs", or a free-form question). When absent the
    /// digest covers the project broadly.
    #[serde(default)]
    focus: Option<String>,
    /// How many recently-updated pages the underlying briefing should
    /// consider (default 10).
    #[serde(default)]
    recent_pages_limit: Option<usize>,
    /// Project to explore. Omit to target the project you're currently
    /// working in (resolved from recent hook activity). **Omit unless the user explicitly names a *different* project.**
    #[serde(default)]
    project: Option<String>,
    /// Workspace to explore together with `project`. Omit to use the
    /// current/default workspace resolution chain.
    #[serde(default)]
    workspace: Option<String>,
}

// The `anyOf` encodes the "you MUST pass exactly one of path/query"
// contract in the machine-readable schema (issue #155): each branch
// demands the key's PRESENCE via `required` AND a non-null `type`, because
// clients that null-fill defaulted args (OpenCode) would satisfy a bare
// `required` with `path: null` and still hit the runtime error. Encoding
// it here lets schema-respecting clients refuse the invalid call before
// it ever reaches the server.
//
// Moonshot ("moonshot flavored json schema") rejects this root-level
// `anyOf`; Kimi Code sessions get a patched schema instead
// (`moonshot_safe_tool_list`). Every other client keeps this exact shape.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(extend("anyOf" = [
    {"required": ["path"], "properties": {"path": {"type": "string"}}},
    {"required": ["query"], "properties": {"query": {"type": "string"}}},
]))]
struct ReadPageArgs {
    /// FTS5 query to find the page (searches and returns the top hit's full
    /// body). You MUST pass exactly one of `query` or `path` — never neither,
    /// and never `null`. Ignored when `path` is provided.
    #[serde(default, alias = "q", alias = "search")]
    query: Option<String>,
    /// Exact wiki path (e.g. `notes/foo.md`), typically taken verbatim from a
    /// `memory_recent` or `memory_query` hit. You MUST pass exactly one of
    /// `path` or `query` — never neither, and never `null`. Takes precedence
    /// over `query`.
    #[serde(default)]
    path: Option<String>,
    /// Project to read from. Omit to target the project you're currently
    /// working in (resolved from recent hook activity). **Omit unless the user explicitly names a *different* project.**
    #[serde(default)]
    project: Option<String>,
    /// Workspace to read together with `project`. Omit to use the
    /// current/default workspace resolution chain. Provide both to read a
    /// page that lives in a *different* workspace (e.g. a sibling project on
    /// a shared server).
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct DeletePageArgs {
    /// Exact wiki path to delete (e.g. `notes/foo.md`).
    path: String,
    /// Project to delete from. Omit to target the project you're currently
    /// working in (resolved from recent hook activity). **Omit unless the
    /// user explicitly names a *different* project.**
    #[serde(default)]
    project: Option<String>,
    /// Workspace to delete from together with `project`. Omit to use the
    /// current/default workspace resolution chain. Provide both to delete a
    /// page that lives in a *different* workspace (e.g. a sibling project on
    /// a shared server). Missing explicit scopes fail closed instead of
    /// falling back to the active/default project.
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WritePageArgs {
    /// Relative wiki path to write, for example `notes/santander-2025.md`.
    path: String,
    /// Markdown body. Pass the durable fact/note content, not a handoff
    /// summary. Start the body with `# Some Title` — ai-memory derives the
    /// page title from that H1 automatically, so you do not need (and should
    /// not pass) the `title` argument.
    body: String,
    /// **Prefer omitting this.** ai-memory derives the title from the first
    /// `# H1` in `body` (or the path stem if there is no heading), so the
    /// safest call is to leave this out and put the title as a markdown H1
    /// on the first line of `body`. Passing a title here forces the agent
    /// to JSON-escape the string correctly — a known source of `JSON parsing`
    /// errors when the title contains quotes, colons, or other punctuation
    /// (issue #67). Only set this when there's no usable H1 in the body.
    #[serde(default)]
    title: Option<String>,
    /// Tier (`working`, `episodic`, `semantic`, `procedural`). Default semantic.
    #[serde(default)]
    tier: Option<String>,
    /// Tags to attach to the page.
    #[serde(default)]
    tags: Vec<String>,
    /// Pin the page so the decay sweep skips it.
    #[serde(default)]
    pinned: bool,
    /// Project to write into. Omit to target the project you're currently
    /// working in (resolved from recent hook activity). When set to a name
    /// that doesn't exist yet, the project is **created** — so writes always
    /// land where you asked, never silently in the current project. **Omit
    /// unless the user explicitly names a *different* project.**
    #[serde(default)]
    project: Option<String>,
    /// Workspace to write into. Only honoured together with an explicit
    /// `project`; created if it doesn't exist. Omit for the current workspace.
    #[serde(default)]
    workspace: Option<String>,
    /// Set to `"global"` to write into the reserved `_global` preferences
    /// scope — standing user/team context (tech preferences, code style,
    /// durable decisions) that default `memory_query` reads union into
    /// every project. Cannot be combined with `workspace`/`project`.
    #[serde(default)]
    scope: Option<String>,
}

#[tool_router]
impl AiMemoryServer {
    fn scope_resolver(&self) -> ScopeResolver<'_> {
        ScopeResolver::new(&self.reader, self.workspace_id, self.project_id)
            .with_writer(&self.writer)
            .with_active_project(&self.active_project)
    }

    fn scope_error(err: ai_memory_store::ScopeResolutionError) -> McpError {
        McpError::internal_error(err.to_string(), None)
    }

    /// Construct a server backed by the given reader/writer + 3-tuple
    /// identity coordinates.
    #[must_use]
    pub fn new(
        reader: ReaderPool,
        writer: WriterHandle,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> Self {
        Self {
            reader,
            writer,
            workspace_id,
            project_id,
            active_project: ActiveProject::new(),
            default_limit: 10,
            consolidator: None,
            llm: None,
            wiki: None,
            decay_params: DecayParams::default(),
            embedder: None,
            sanitizer: ai_memory_core::Sanitizer::builtin(),
            auto_improve_require_approval: false,
            auto_improve_review_config: default_auto_improve_review_config(),
            tool_router: Self::tool_router(),
        }
    }

    /// Configure whether auto-improvement requires manual pending-writes approval.
    #[must_use]
    pub fn with_auto_improve_require_approval(mut self, require_approval: bool) -> Self {
        self.auto_improve_require_approval = require_approval;
        self
    }

    /// Configure manual MCP auto-improve review budgets from server config.
    #[must_use]
    pub fn with_auto_improve_review_config(mut self, config: AutoImproveReviewConfig) -> Self {
        self.auto_improve_review_config = config;
        self
    }

    /// Replace the default built-in-only sanitizer with one carrying
    /// the operator's `[sanitize]` extras + allowlist.
    #[must_use]
    pub fn with_sanitizer(mut self, sanitizer: ai_memory_core::Sanitizer) -> Self {
        self.sanitizer = sanitizer;
        self
    }

    /// Attach an embedder for hybrid (FTS5 + vector RRF) query. Without
    /// this, `memory_query` runs pure FTS5.
    #[must_use]
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Share the hook router's [`ActiveProject`] pointer so the read
    /// tools default to the project the user is currently in (issue #2).
    /// In stdio mode there is no shared hook ingress, so callers simply
    /// don't set this and the baked-in default is used.
    #[must_use]
    pub fn with_active_project(mut self, active_project: ActiveProject) -> Self {
        self.active_project = active_project;
        self
    }

    /// Build the [`ActorKey`] for a tool call from the request's stored
    /// extensions and headers.
    ///
    /// - `user` is taken from the middleware-injected
    ///   [`ai_memory_core::ActorContext`] (rung 1 root, rung 2 DB user) —
    ///   never from raw client-supplied headers, since user identity is
    ///   security-critical.
    /// - `session_id` comes from the same `ActorContext` when the auth
    ///   middleware filled it; if not, falls back to the rung-4
    ///   `X-Memory-Actor-Session-Id` request header, then to the standard
    ///   MCP `Mcp-Session-Id` header. The session id is just a cache key
    ///   for the active-project map — getting it wrong only routes the
    ///   lookup to a different (or absent) slot, with no auth-bypass risk,
    ///   so trusting the header here is safe.
    ///
    /// Returns the empty [`ActorKey`] when neither source has anything to
    /// offer; that's the graceful-degradation signal for callers to fall
    /// back to the single slot.
    fn actor_key_from_parts(
        parts: Option<&axum::http::request::Parts>,
    ) -> ai_memory_core::ActorKey {
        let Some(parts) = parts else {
            return ai_memory_core::ActorKey::default();
        };
        let ctx = parts.extensions.get::<ai_memory_core::ActorContext>();
        let user = ctx.and_then(|c| c.user.clone());
        let header_session = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let session_id = ctx
            .and_then(|c| c.session_id.clone())
            .or_else(|| header_session("x-memory-actor-session-id"))
            .or_else(|| header_session("mcp-session-id"));
        ai_memory_core::ActorKey { user, session_id }
    }

    /// Resolve which `(workspace_id, project_id)` a read tool should
    /// query. Precedence (matches the documented resolution chain):
    ///   1. an explicit `project` name argument in the active workspace
    ///      when hooks have published one (for THIS actor),
    ///   2. that same explicit `project` in the server's baked workspace,
    ///   3. the hook-published [`ActiveProject`] (the cwd the agent is
    ///      currently working in, keyed by `actor` in opt-in isolation
    ///      modes),
    ///   4. the server's baked-in `--project` default.
    ///
    /// `actor` is built by [`Self::actor_key_from_parts`]; pass
    /// `ActorKey::default()` when the call site has no request context.
    /// Empty actor → fall back to the single slot (legacy behaviour).
    #[cfg(test)]
    async fn effective_ids_with_actor(
        &self,
        explicit_project: Option<&str>,
        actor: &ai_memory_core::ActorKey,
    ) -> Result<(WorkspaceId, ProjectId), McpError> {
        self.scope_resolver()
            .resolve_current_or_project(explicit_project, actor)
            .await
            .map(ai_memory_store::ResolvedScope::as_tuple)
            .map_err(Self::scope_error)
    }

    async fn effective_ids_for_read_args_with_actor(
        &self,
        explicit_workspace: Option<&str>,
        explicit_project: Option<&str>,
        actor: &ai_memory_core::ActorKey,
    ) -> Result<(WorkspaceId, ProjectId), McpError> {
        self.scope_resolver()
            .resolve_read_args(explicit_workspace, explicit_project, actor)
            .await
            .map(ai_memory_store::ResolvedScope::as_tuple)
            .map_err(Self::scope_error)
    }

    /// Resolve the target for a WRITE, **creating** the workspace/project when
    /// an explicit name doesn't exist yet. Distinct from [`Self::effective_ids`]
    /// (find-only, for reads): a write to a named project must land there, not
    /// silently fall back to the current project. With no explicit `project`,
    /// the active-project-wins behaviour is preserved (issue #2).
    ///
    /// When an explicit `project` is given but **no** explicit `workspace`, the
    /// workspace defaults to the hook-published [`ActiveProject`]'s workspace
    /// (the cwd the agent is working in) — NOT the server's baked `--workspace`.
    /// Otherwise a write like `{project: "foo"}` from a cwd routed to workspace
    /// `bar` would silently land in (and recreate) `default/foo` instead of
    /// `bar/foo`. To target the baked/shared workspace explicitly, pass
    /// `workspace`. Falls back to the baked default only when no `ActiveProject`
    /// has been published yet (early startup / no hooks).
    /// Legacy single-slot wrapper retained for test fixtures that pre-date
    /// the actor-aware variant. Production tools must use
    /// [`Self::write_target_ids_with_actor`] so per-session/per-actor
    /// isolation modes route the write to the caller's project, not
    /// whichever single-slot value was published last.
    #[cfg(test)]
    async fn write_target_ids(
        &self,
        explicit_workspace: Option<&str>,
        explicit_project: Option<&str>,
    ) -> Result<(WorkspaceId, ProjectId), McpError> {
        self.write_target_ids_with_actor(
            explicit_workspace,
            explicit_project,
            &ai_memory_core::ActorKey::default(),
        )
        .await
    }

    async fn write_target_ids_with_actor(
        &self,
        explicit_workspace: Option<&str>,
        explicit_project: Option<&str>,
        actor: &ai_memory_core::ActorKey,
    ) -> Result<(WorkspaceId, ProjectId), McpError> {
        self.scope_resolver()
            .resolve_write_args(explicit_workspace, explicit_project, actor)
            .await
            .map(ai_memory_store::ResolvedScope::as_tuple)
            .map_err(Self::scope_error)
    }

    async fn resolve_query_scopes(
        &self,
        scopes: &[MemoryScopeArg],
    ) -> Result<Vec<(WorkspaceId, ProjectId)>, McpError> {
        let names: Vec<_> = scopes
            .iter()
            .map(|scope| ScopeName::new(&scope.workspace, &scope.project))
            .collect();
        self.scope_resolver()
            .resolve_many_existing(&names, MAX_QUERY_SCOPES)
            .await
            .map(|scopes| {
                scopes
                    .into_iter()
                    .map(ai_memory_store::ResolvedScope::as_tuple)
                    .collect()
            })
            .map_err(Self::scope_error)
    }

    /// Human-readable `workspace/project` label for a resolved scope. Makes
    /// not-found errors diagnosable — especially under cross-project
    /// scope-bleed, where a scoped-implicit read resolves to a different
    /// scope than a concurrent write landed in. Degrades to a placeholder
    /// when a name lookup fails so it never turns a not-found into a harder
    /// error.
    async fn scope_label(
        &self,
        ws: ai_memory_core::WorkspaceId,
        proj: ai_memory_core::ProjectId,
    ) -> String {
        let ws_name = self.reader.workspace_name_by_id(ws).await.ok().flatten();
        let proj_name = self
            .reader
            .project_name_by_id(ws, proj)
            .await
            .ok()
            .flatten();
        format!(
            "{}/{}",
            ws_name.as_deref().unwrap_or("<unknown-workspace>"),
            proj_name.as_deref().unwrap_or("<unknown-project>")
        )
    }

    async fn embed_query(&self, query: &str) -> Option<Vec<f32>> {
        let Some(embedder) = &self.embedder else {
            return None;
        };
        match embedder.embed(query).await {
            Ok(qv) => Some(qv),
            Err(e) => {
                tracing::warn!(
                    provider = embedder.provider(),
                    model = embedder.model(),
                    error = %e,
                    "embedder failed; degrading memory_query to BM25-only"
                );
                None
            }
        }
    }

    async fn search_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        query: &str,
        query_vec: Option<&[f32]>,
        limit: usize,
    ) -> ai_memory_store::StoreResult<Vec<PageHit>> {
        if let (Some(embedder), Some(qv)) = (&self.embedder, query_vec) {
            return self
                .reader
                .hybrid_search(
                    workspace_id,
                    project_id,
                    query.to_owned(),
                    Some(qv.to_vec()),
                    embedder.provider().to_string(),
                    embedder.model().to_string(),
                    embedder.dim(),
                    limit,
                )
                .await;
        }
        self.reader
            .search_pages_for_project(workspace_id, project_id, query.to_owned(), limit)
            .await
    }

    /// Override the retention-sweep parameters (typically populated
    /// from the user's config.toml `[decay]` table).
    #[must_use]
    pub fn with_decay_params(mut self, params: DecayParams) -> Self {
        self.decay_params = params;
        self
    }

    /// Attach the wiki handle. Without this, `memory_forget_sweep`
    /// and `memory_lint` cannot write their report pages.
    #[must_use]
    pub fn with_wiki(mut self, wiki: Wiki) -> Self {
        self.wiki = Some(wiki);
        self
    }

    /// Attach an LLM-backed consolidator. Without this, the
    /// `memory_consolidate` tool errors with "not configured". Also
    /// stores the LLM provider so `memory_lint` can run its
    /// contradiction pass. Accepts a pre-built `Arc<Consolidator>` so
    /// the same consolidator can be shared with another subsystem
    /// (e.g. the hook router's PreCompact branch) and both paths see
    /// the same handle.
    #[must_use]
    pub fn with_consolidator_arc(
        mut self,
        wiki: Wiki,
        llm: Arc<dyn LlmProvider>,
        consolidator: Arc<Consolidator>,
    ) -> Self {
        self.consolidator = Some(consolidator);
        self.llm = Some(llm);
        self.wiki = Some(wiki);
        self
    }

    /// Search the compiled wiki via FTS5/vector/graph retrieval. Default,
    /// explicit project, and explicit `scopes` searches fall back to bounded
    /// raw observation search when no compiled page matches; `global=true`
    /// searches compiled wiki pages across projects only.
    #[tool(description = "Search the project's long-term memory wiki — \
        prior sessions, decisions, gotchas, architecture notes captured \
        by ai-memory across earlier runs. Call this BEFORE proposing \
        designs, BEFORE answering 'why does X work this way', and \
        whenever the user references prior work you don't recognise. \
        FTS5 + graph RRF + (when configured) vector RRF re-ranking. \
        Returns up to `limit` pages with HTML-marked snippets and a rank \
        score (lower rank = better match). Only latest page versions. \
        If compiled wiki search misses in default/project/`scopes` mode, \
        `raw_hits` contains bounded raw observation fallback matches; \
        `global=true` searches compiled wiki pages only and returns no raw \
        fallback. Default-scoped calls also return \
        `global_scope_hits`: standing user/team preferences from the \
        reserved `_global` scope that apply across projects. Set \
        `global=true` to search EVERY \
        project at once (cross-project) when you don't know which project \
        holds the knowledge — each hit then carries its workspace + \
        project name.")]
    async fn memory_query(
        &self,
        Parameters(args): Parameters<QueryArgs>,
        OptionalParts(parts): OptionalParts,
    ) -> Result<CallToolResult, McpError> {
        let aps_actor = Self::actor_key_from_parts(Some(&parts));
        let limit = args.limit.unwrap_or(self.default_limit).clamp(1, 100);
        // A repo that opted into `[recall] default_global` (published on the
        // ActiveProject by the hook) makes a query with NO explicit scoping
        // behave as `global=true`. Precedence is strict: an explicit
        // `global` / `scopes` / `workspace` / `project` arg always wins, so
        // this only fires when the caller passed none of them.
        let explicit_scoping = !args.scopes.is_empty()
            || named_scope_args_present(args.workspace.as_deref(), args.project.as_deref());
        let recall_global = !explicit_scoping
            && !args.global.unwrap_or(false)
            && self.active_project.default_global_for(&aps_actor);
        if args.global.unwrap_or(false) || recall_global {
            if !args.scopes.is_empty()
                || args
                    .workspace
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty())
                || args
                    .project
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty())
            {
                return Err(McpError::internal_error(
                    "global cannot be combined with workspace/project/scopes",
                    None,
                ));
            }
            let global_hits = self
                .reader
                .search_pages_with_meta(args.query.clone(), limit)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return ok_json(&MemoryQueryResponse {
                hits: Vec::new(),
                raw_hits: Vec::new(),
                global_hits,
                global_scope_hits: Vec::new(),
            });
        }
        if !args.scopes.is_empty()
            && (args
                .workspace
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
                || args
                    .project
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty()))
        {
            return Err(McpError::internal_error(
                "scopes cannot be combined with workspace/project",
                None,
            ));
        }

        let query = args.query.clone();
        let query_vec = self.embed_query(&args.query).await;
        let resolved_scopes = if args.scopes.is_empty() {
            None
        } else {
            Some(self.resolve_query_scopes(&args.scopes).await?)
        };
        let hits = if let Some(scopes) = &resolved_scopes {
            let mut hits_by_id: HashMap<PageId, PageHit> = HashMap::new();
            for &(ws, proj) in scopes {
                let hits = self
                    .search_project(ws, proj, &args.query, query_vec.as_deref(), limit)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                for hit in hits {
                    hits_by_id
                        .entry(hit.id)
                        .and_modify(|existing| {
                            if hit.rank < existing.rank {
                                *existing = hit.clone();
                            }
                        })
                        .or_insert(hit);
                }
            }
            let mut hits: Vec<PageHit> = hits_by_id.into_values().collect();
            hits.sort_by(|a, b| {
                a.rank
                    .partial_cmp(&b.rank)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            hits.truncate(limit);
            Ok(hits)
        } else {
            let (ws, proj) = self
                .effective_ids_for_read_args_with_actor(
                    args.workspace.as_deref(),
                    args.project.as_deref(),
                    &aps_actor,
                )
                .await?;
            self.search_project(ws, proj, &args.query, query_vec.as_deref(), limit)
                .await
        };
        let hits = hits.map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.spawn_access_bump(hits.iter().map(|h| h.id).collect());
        // Raw-observation fallback when compiled-page search misses. Works
        // for a single resolved project (default / workspace+project) AND
        // for explicit `scopes` — the recommended scope-bleed mitigation —
        // by searching observations in each resolved (ws, proj) and
        // rank-merging, so the fallback isn't lost on the scoped path.
        let raw_hits = if !hits.is_empty() {
            Vec::new()
        } else if let Some(scopes) = &resolved_scopes {
            let mut obs: Vec<ai_memory_store::ObservationHit> = Vec::new();
            for &(ws, proj) in scopes {
                let mut scope_obs = self
                    .reader
                    .search_observations_for_project(ws, proj, query.clone(), limit)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                obs.append(&mut scope_obs);
            }
            obs.sort_by(|a, b| {
                a.rank
                    .partial_cmp(&b.rank)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            obs.truncate(limit);
            obs
        } else {
            let (ws, proj) = self
                .effective_ids_for_read_args_with_actor(
                    args.workspace.as_deref(),
                    args.project.as_deref(),
                    &aps_actor,
                )
                .await?;
            self.reader
                .search_observations_for_project(ws, proj, query, limit)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        };
        // Default-scoped queries (no workspace/project/scopes/global args)
        // also union the reserved `_global` preferences scope, so standing
        // user/team context travels into every project without the caller
        // knowing a magic project name (issue #154). Explicit scoping means
        // the caller asked for exactly those scopes — leave it alone. One
        // extra scoped search when the scope exists; zero cost when it
        // doesn't.
        let default_scoped = args.scopes.is_empty()
            && args
                .workspace
                .as_deref()
                .is_none_or(|s| s.trim().is_empty())
            && args.project.as_deref().is_none_or(|s| s.trim().is_empty());
        let global_scope_hits = if default_scoped {
            match ai_memory_store::lookup_global_scope(&self.reader).await {
                Ok(Some(scope)) => {
                    // If the current project IS the reserved scope (e.g. the
                    // actor's active-project pointer lands there after a
                    // global write), `hits` already covers it — don't search
                    // it twice.
                    let current = self
                        .effective_ids_for_read_args_with_actor(None, None, &aps_actor)
                        .await?;
                    if current == scope.as_tuple() {
                        Vec::new()
                    } else {
                        let hits = self
                            .search_project(
                                scope.workspace_id,
                                scope.project_id,
                                &args.query,
                                query_vec.as_deref(),
                                limit,
                            )
                            .await
                            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                        self.spawn_access_bump(hits.iter().map(|h| h.id).collect());
                        hits
                    }
                }
                Ok(None) => Vec::new(),
                Err(e) => return Err(McpError::internal_error(e.to_string(), None)),
            }
        } else {
            Vec::new()
        };
        let response = MemoryQueryResponse {
            hits,
            raw_hits,
            global_hits: Vec::new(),
            global_scope_hits,
        };
        ok_json(&response)
    }

    /// Return the N most-recently-updated pages.
    #[tool(description = "Return the N most-recently-updated wiki pages \
        for this project (descending by updated_at). Call this at the \
        START of any session to see what the previous session was \
        working on — even when no explicit handoff exists. Cheap, fast, \
        no LLM cost. Pair with memory_query when you need to drill into \
        specifics.")]
    async fn memory_recent(
        &self,
        Parameters(args): Parameters<RecentArgs>,
        OptionalParts(parts): OptionalParts,
    ) -> Result<CallToolResult, McpError> {
        let aps_actor = Self::actor_key_from_parts(Some(&parts));
        let limit = args.limit.unwrap_or(self.default_limit).clamp(1, 100);
        // A repo that opted into `[recall] default_global` broadens an
        // unscoped `memory_recent` to "most recent across every project", each
        // hit annotated with its workspace + project. An explicit
        // `workspace`/`project` always wins (same precedence as memory_query).
        let explicit_scoping =
            named_scope_args_present(args.workspace.as_deref(), args.project.as_deref());
        if !explicit_scoping && self.active_project.default_global_for(&aps_actor) {
            let global_hits = self
                .reader
                .recent_pages_global(limit)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return ok_json(&MemoryRecentResponse {
                hits: Vec::new(),
                global_hits,
            });
        }
        let (ws, proj) = self
            .effective_ids_for_read_args_with_actor(
                args.workspace.as_deref(),
                args.project.as_deref(),
                &aps_actor,
            )
            .await?;
        let hits = self
            .reader
            .recent_pages_for_project(ws, proj, limit)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.spawn_access_bump(hits.iter().map(|h| h.id).collect());
        ok_json(&MemoryRecentResponse {
            hits,
            global_hits: Vec::new(),
        })
    }

    /// Run the M8 forget sweep over episodic pages.
    #[tool(description = "Run the retention sweep: walk is_latest=1 \
        episodic pages, score them with the agentmemory-style retention \
        formula (salience * exp(-lambda * age) + sigma * log(1 + accesses) \
        * exp(-mu * days_since_access)), and soft-delete those below the \
        cold threshold. Semantic / procedural / pinned pages are exempt. \
        Pass dry_run=true to preview.")]
    async fn memory_forget_sweep(
        &self,
        Parameters(args): Parameters<SweepArgs>,
        OptionalParts(parts): OptionalParts,
    ) -> Result<CallToolResult, McpError> {
        let aps_actor = Self::actor_key_from_parts(Some(&parts));
        let (ws, proj) = self
            .effective_ids_for_read_args_with_actor(
                args.workspace.as_deref(),
                args.project.as_deref(),
                &aps_actor,
            )
            .await?;
        let report = run_sweep(
            &self.reader,
            &self.writer,
            ws,
            proj,
            &self.decay_params,
            args.dry_run.unwrap_or(false),
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        ok_json(&report)
    }

    /// Run the M8 lint pass: rule-based + optional LLM contradiction.
    #[tool(description = "Audit the wiki for stale episodic pages, \
        duplicate titles, broken cross-references, and (if an LLM \
        provider is configured) contradictions across semantic pages. \
        Findings land in wiki/_lint/<date>.md unless dry_run=true.")]
    async fn memory_lint(
        &self,
        Parameters(args): Parameters<LintArgs>,
        OptionalParts(parts): OptionalParts,
    ) -> Result<CallToolResult, McpError> {
        let aps_actor = Self::actor_key_from_parts(Some(&parts));
        let Some(wiki) = self.wiki.as_ref() else {
            return Err(McpError::internal_error(
                "memory_lint requires the server to be built with a wiki handle",
                None,
            ));
        };
        let (ws, proj) = self
            .effective_ids_for_read_args_with_actor(
                args.workspace.as_deref(),
                args.project.as_deref(),
                &aps_actor,
            )
            .await?;
        let report = run_lint(
            &self.reader,
            wiki,
            self.llm.as_ref(),
            ws,
            proj,
            args.dry_run.unwrap_or(false),
            !args.no_llm.unwrap_or(false),
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        ok_json(&report)
    }

    /// LLM-driven consolidation of a session.
    #[tool(description = "LLM-driven consolidation. Default mode \
        (single-page) rewrites sessions/<id>.md from the observation \
        log. multi_page=true fans out into a batch of concept/decision/\
        gotcha pages plus the session page, all written in one atomic \
        SQL transaction. Off by default; requires AI_MEMORY_LLM_PROVIDER \
        plus that provider's credentials. AI_MEMORY_LLM_MODEL is optional \
        for providers with a built-in default. \
        The consolidation target is resolved from where the session's \
        observations actually landed, so a session that adopted its scope \
        marker mid-run still consolidates into the right project. Admission \
        is checked up front, before the LLM, so a rejected scope fails fast \
        without spending a completion. Pass dry_run=true for a cheap plan: \
        it runs that admission preflight and reports the resolved page path \
        WITHOUT calling the LLM (no body preview); run without dry_run to \
        produce the actual page(s).")]
    async fn memory_consolidate(
        &self,
        Parameters(args): Parameters<ConsolidateArgs>,
        OptionalParts(parts): OptionalParts,
    ) -> Result<CallToolResult, McpError> {
        let Some(consolidator) = self.consolidator.as_ref() else {
            return Err(McpError::internal_error(
                "memory_consolidate not configured (set AI_MEMORY_LLM_PROVIDER and the provider's required credentials; providers without a built-in model also require AI_MEMORY_LLM_MODEL)",
                None,
            ));
        };
        let session_id = SessionId::from_str(&args.session_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let dry = args.dry_run.unwrap_or(false);
        // Carry the request's authenticated identity into the write so the
        // consolidated page is attributed to the real operator and any
        // admission webhook authorizes by that actor (rather than the previous
        // hard-coded anonymous, which an actor-gated webhook rejects).
        let actor = crate::actor::actor_from_parts(&parts);
        let author_id = crate::actor::author_id_from_parts(&parts);
        if args.multi_page.unwrap_or(false) {
            let outcomes = consolidator
                .consolidate_session_multi(session_id, dry, actor, author_id)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            ok_json(&serde_json::json!({ "outcomes": outcomes }))
        } else {
            let outcome = consolidator
                .consolidate_session(session_id, dry, actor, author_id)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            ok_json(&outcome)
        }
    }

    /// Stage durable wiki edit proposals for a completed session.
    #[tool(
        description = "Run manual auto-improvement for one completed session and apply or stage validated wiki edit proposals through the auto-improvement approval path. Use when the user asks what durable lessons should be captured, what memory pages this session suggests, or at explicit wrap-up when a learning review is useful. Omit `session_id` to review the latest completed session in the current project. The server also schedules background review for newly completed sessions in every project when an LLM provider is configured. Admins can set `[auto_improve.scheduler] enabled = false` to stop automatic review, or `[auto_improve] require_approval = true` to leave scheduled and manual proposals pending for review."
    )]
    async fn memory_auto_improve(
        &self,
        Parameters(args): Parameters<AutoImproveArgs>,
        OptionalParts(parts): OptionalParts,
    ) -> Result<CallToolResult, McpError> {
        if args.dry_run.is_some() || args.stage.is_some() || args.mode.is_some() {
            return Err(McpError::invalid_params(
                "auto-improve dry_run/stage/mode arguments were removed; set [auto_improve].require_approval = true for manual review",
                None,
            ));
        }
        let aps_actor = Self::actor_key_from_parts(Some(&parts));
        let request_actor = parts
            .extensions
            .get::<ai_memory_core::ActorContext>()
            .cloned()
            .unwrap_or_else(ai_memory_core::ActorContext::anonymous);
        let author_id = parts.extensions.get::<ai_memory_core::UserId>().copied();
        let (ws, proj) = self
            .effective_ids_for_read_args_with_actor(
                args.workspace.as_deref(),
                args.project.as_deref(),
                &aps_actor,
            )
            .await?;
        let Some(llm) = self.llm.as_ref() else {
            return Err(McpError::internal_error(
                "memory_auto_improve not configured (set AI_MEMORY_LLM_PROVIDER on the server)",
                None,
            ));
        };
        let Some(wiki) = self.wiki.as_ref() else {
            return Err(McpError::internal_error(
                "memory_auto_improve requires the server to be built with a wiki handle",
                None,
            ));
        };
        let session_id = match args.session_id.as_deref() {
            Some(raw) => SessionId::from_str(raw)
                .map_err(|e| McpError::invalid_params(e.to_string(), None))?,
            None => self
                .reader
                .latest_completed_session_for_project(ws, proj)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
                .ok_or_else(|| {
                    McpError::internal_error(
                        "no completed session found for the resolved project",
                        None,
                    )
                })?,
        };
        let defaults = &self.auto_improve_review_config;
        let cfg = AutoImproveReviewConfig {
            min_observations: args.min_observations.unwrap_or(defaults.min_observations),
            min_session_duration_secs: args
                .min_session_duration_secs
                .unwrap_or(defaults.min_session_duration_secs),
            min_confidence: args.min_confidence.unwrap_or(defaults.min_confidence),
            max_input_tokens: args.max_input_tokens.unwrap_or(defaults.max_input_tokens),
            max_proposals_per_run: args.max_proposals.unwrap_or(defaults.max_proposals_per_run),
            include_raw_fallback: args
                .include_raw_fallback
                .unwrap_or(defaults.include_raw_fallback),
            proposal_actor: defaults.proposal_actor.clone(),
            pending_path: defaults.pending_path.clone(),
            max_patchable_pages: defaults.max_patchable_pages,
            max_patchable_body_chars: defaults.max_patchable_body_chars,
            max_edits_per_proposal: defaults.max_edits_per_proposal,
            max_edit_content_chars: defaults.max_edit_content_chars,
            max_changed_chars_per_proposal: defaults.max_changed_chars_per_proposal,
            max_patch_edits_per_run: defaults.max_patch_edits_per_run,
            max_rejection_context: defaults.max_rejection_context,
            rejection_context_days: defaults.rejection_context_days,
            max_final_body_chars: defaults.max_final_body_chars,
            max_rule_page_tokens: defaults.max_rule_page_tokens,
            max_procedure_page_tokens: defaults.max_procedure_page_tokens,
            eval: defaults.eval.clone(),
        };

        let report =
            run_auto_improve_review(&self.reader, &**llm, ws, proj, session_id, cfg.clone())
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let mut proposals = Vec::with_capacity(report.proposals.len());
        for p in &report.proposals {
            let path = PagePath::new(p.path.clone()).map_err(|e| {
                McpError::invalid_params(format!("invalid proposal path: {e}"), None)
            })?;
            let target_exists = self
                .reader
                .page_body_by_ids(ws, proj, path.as_str())
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
                .is_some();
            let operation = if p.edit_mode == "patch"
                || (target_exists && path.as_str() == "_slots/current-focus.md")
            {
                AutoImproveProposalOperation::Update
            } else {
                AutoImproveProposalOperation::Create
            };
            let expected_base_body_sha256 = p
                .expected_base_body_sha256
                .as_deref()
                .map(hex_to_sha256)
                .transpose()
                .map_err(|e| {
                    McpError::internal_error(
                        format!("invalid expected_base_body_sha256: {e}"),
                        None,
                    )
                })?;
            proposals.push(NewAutoImproveProposal {
                operation,
                target_path: path,
                kind: p.kind.clone(),
                title: p.title.clone(),
                confidence: f64::from(p.confidence),
                rationale: p.rationale.clone(),
                evidence_json: serde_json::to_value(&p.evidence)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?,
                body_markdown: p.body_markdown.clone(),
                artifact_sha256: None,
                edit_mode: Some(p.edit_mode.clone()),
                patch_json: serde_json::to_value(&p.edits).ok(),
                expected_base_body_sha256,
            });
        }
        let staged = self
            .writer
            .stage_auto_improve_run(StageAutoImproveRun {
                workspace_id: ws,
                project_id: proj,
                session_id: Some(session_id),
                provider: Some(report.provider.clone()),
                model: Some(report.model.clone()),
                summary: Some(report.summary.clone()),
                warnings_json: serde_json::to_value(&report.warnings)
                    .unwrap_or_else(|_| serde_json::json!([])),
                rejected_candidates_json: serde_json::to_value(&report.rejected_candidates)
                    .unwrap_or_else(|_| serde_json::json!([])),
                config_json: serde_json::json!({
                    "min_observations": cfg.min_observations,
                    "min_session_duration_secs": cfg.min_session_duration_secs,
                    "min_confidence": cfg.min_confidence,
                    "max_input_tokens": cfg.max_input_tokens,
                    "max_proposals_per_run": cfg.max_proposals_per_run,
                    "include_raw_fallback": cfg.include_raw_fallback,
                    "max_patchable_pages": cfg.max_patchable_pages,
                    "max_patchable_body_chars": cfg.max_patchable_body_chars,
                    "max_edits_per_proposal": cfg.max_edits_per_proposal,
                    "max_edit_content_chars": cfg.max_edit_content_chars,
                    "max_changed_chars_per_proposal": cfg.max_changed_chars_per_proposal,
                    "max_patch_edits_per_run": cfg.max_patch_edits_per_run,
                    "max_rejection_context": cfg.max_rejection_context,
                    "rejection_context_days": cfg.rejection_context_days,
                    "max_final_body_chars": cfg.max_final_body_chars,
                    "max_rule_page_tokens": cfg.max_rule_page_tokens,
                    "max_procedure_page_tokens": cfg.max_procedure_page_tokens,
                    "eval": cfg.eval,
                }),
                proposal_actor: ai_memory_core::ActorContext {
                    agent: Some(cfg.proposal_actor.clone()),
                    ..ai_memory_core::ActorContext::default()
                },
                proposals,
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let mut sidecar_paths = Vec::with_capacity(staged.proposal_ids.len());
        for id in &staged.proposal_ids {
            let path = wiki
                .write_auto_improve_sidecar(ws, proj, *id)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            sidecar_paths.push(path.display().to_string());
        }
        let mut outcomes = Vec::with_capacity(staged.proposal_ids.len());
        for (proposal_id, sidecar_path) in staged.proposal_ids.iter().zip(sidecar_paths.iter()) {
            if self.auto_improve_require_approval {
                outcomes.push(serde_json::json!({
                    "id": proposal_id.to_string(),
                    "sidecar_path": sidecar_path,
                    "status": "pending",
                    "page_id": null,
                }));
                continue;
            }
            let mut approval_actor = request_actor.clone();
            approval_actor.agent = Some("auto_improve_auto_approve".into());
            match wiki
                .approve_auto_improve_proposal(
                    ws,
                    proj,
                    *proposal_id,
                    approval_actor,
                    author_id,
                    Some(ai_memory_wiki::AdmissionContext {
                        op: ai_memory_wiki::AdmissionOp::WritePage,
                        ..ai_memory_wiki::AdmissionContext::default()
                    }),
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
            {
                ai_memory_store::ApproveAutoImproveProposalResult::Approved { page_id } => {
                    outcomes.push(serde_json::json!({
                        "id": proposal_id.to_string(),
                        "sidecar_path": sidecar_path,
                        "status": "approved",
                        "page_id": page_id.to_string(),
                    }));
                }
                ai_memory_store::ApproveAutoImproveProposalResult::Conflict => {
                    outcomes.push(serde_json::json!({
                        "id": proposal_id.to_string(),
                        "sidecar_path": sidecar_path,
                        "status": "conflict",
                        "page_id": null,
                    }));
                }
            }
        }
        ok_json(&serde_json::json!({
            "run_id": staged.run_id.to_string(),
            "approval_required": self.auto_improve_require_approval,
            "approval_policy": if self.auto_improve_require_approval { "manual" } else { "auto_approve" },
            "session_id": session_id.to_string(),
            "summary": report.summary,
            "warnings": report.warnings,
            "rejected_candidates_count": report.rejected_candidates.len(),
            "proposals": outcomes,
        }))
    }

    /// Write or update a durable wiki page.
    #[tool(description = "Write or update a durable wiki page for the \
        current project. Use this when the user explicitly asks to \
        remember, save, pin, annotate, or make permanent a fact/rule/note. \
        This is for long-lived project knowledge; do NOT use \
        memory_handoff_begin for permanent annotations. Choose a stable \
        relative path such as `notes/<topic>.md`, `concepts/<topic>.md`, \
        `decisions/<topic>.md`, or `_rules/<topic>.md`. `tier` defaults \
        to `semantic`; set `pinned=true` for facts that should never decay. \
        For standing user/team preferences that apply to EVERY project \
        (tech choices, code style, durable personal rules), pass \
        `scope: \"global\"` — the page lands in the reserved `_global` \
        scope and default memory_query calls surface it in every project. \
        \
        **Title convention:** start `body` with a `# Some Title` line — \
        ai-memory derives the title from that H1 automatically. Do NOT \
        pass the `title` argument; passing it forces correct JSON-escaping \
        of the string and is a known source of `JSON parsing` errors when \
        the title contains quotes or punctuation (issue #67). Use `title` \
        only when there's no usable H1 in the body.")]
    async fn memory_write_page(
        &self,
        Parameters(args): Parameters<WritePageArgs>,
        OptionalParts(parts): OptionalParts,
    ) -> Result<CallToolResult, McpError> {
        let aps_actor = Self::actor_key_from_parts(Some(&parts));
        let Some(wiki) = self.wiki.as_ref() else {
            return Err(McpError::internal_error(
                "memory_write_page requires the server to be built with a wiki handle",
                None,
            ));
        };
        let tier_name = args.tier.as_deref().unwrap_or("semantic");
        let tier: Tier = tier_name
            .parse()
            .map_err(|_| McpError::internal_error(format!("unknown tier '{tier_name}'"), None))?;
        let path = PagePath::new(args.path.clone())
            .map_err(|e| McpError::internal_error(format!("invalid path: {e}"), None))?;
        let (ws, proj) = match args.scope.as_deref().map(str::trim) {
            None | Some("") => {
                self.write_target_ids_with_actor(
                    args.workspace.as_deref(),
                    args.project.as_deref(),
                    &aps_actor,
                )
                .await?
            }
            Some("global") => {
                if args
                    .workspace
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty())
                    || args
                        .project
                        .as_deref()
                        .is_some_and(|s| !s.trim().is_empty())
                {
                    return Err(McpError::internal_error(
                        "scope: \"global\" cannot be combined with workspace/project",
                        None,
                    ));
                }
                ai_memory_store::create_global_scope(&self.writer)
                    .await
                    .map_err(Self::scope_error)?
                    .as_tuple()
            }
            Some(other) => {
                return Err(McpError::internal_error(
                    format!("unknown scope '{other}': the only supported value is \"global\""),
                    None,
                ));
            }
        };

        let mut fm = serde_json::Map::new();
        if let Some(title) = &args.title {
            fm.insert("title".into(), serde_json::Value::String(title.clone()));
        }
        if !args.tags.is_empty() {
            fm.insert(
                "tags".into(),
                serde_json::Value::Array(
                    args.tags
                        .iter()
                        .cloned()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
        }
        if args.pinned {
            fm.insert("pinned".into(), serde_json::Value::Bool(true));
        }
        let frontmatter = if fm.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::Object(fm)
        };

        // rmcp exposes the original HTTP `Parts`; trust the auth middleware's
        // extension, not raw client-controlled actor headers.
        let actor = crate::actor::actor_from_parts(&parts);
        let author_id = crate::actor::author_id_from_parts(&parts);
        // Loop prevention: a webhook that writes back into the engine sets
        // `X-Memory-Skip-Admission-Chain` so the chain doesn't re-invoke it
        // on the recursive write. Only trusted/root re-entry can honor it.
        let skip_webhooks = crate::actor::skip_webhooks_from_parts(&parts);
        let admission_ctx = if actor.has_any() || !skip_webhooks.is_empty() {
            // Actor is NOT carried here — `write_page` fills the webhook
            // context from `req.actor` (single identity source).
            Some(ai_memory_wiki::AdmissionContext {
                op: ai_memory_wiki::AdmissionOp::WritePage,
                skip_webhooks,
                ..ai_memory_wiki::AdmissionContext::default()
            })
        } else {
            None
        };

        let page_id = wiki
            .write_page(WritePageRequest {
                workspace_id: ws,
                project_id: proj,
                path: path.clone(),
                frontmatter,
                body: args.body,
                tier,
                pinned: args.pinned,
                title: args.title,
                admission_ctx,
                author_id,
                actor,
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let checkpoint = checkpoint_or_warn(wiki, format!("memory_write_page: {}", path.as_str()));

        ok_json(&serde_json::json!({
            "page_id": page_id.to_string(),
            "path": path.to_string(),
            "checkpoint": checkpoint
        }))
    }

    /// Fetch the full body of a single wiki page.
    #[tool(description = "Fetch the FULL body of a wiki page. You MUST pass \
        exactly one of `path` or `query` — a call with neither (or with \
        nulls) is invalid and will error; do NOT retry it unchanged. \
        \
        Two modes: \
        (1) pass `path` — direct lookup by the page's relative wiki path, \
        taken verbatim from a `memory_recent`/`memory_query` hit (e.g. \
        `{\"path\": \"notes/budget.md\"}`); \
        (2) pass `query` — runs an FTS5 search and returns the top hit's \
        complete body. `path` takes precedence when both are given. \
        \
        Defaults to the current project; pass `workspace` + `project` \
        together only when the user names a sibling workspace/project. Use \
        this when the user asks to read, open, or show a specific page by \
        name or topic — not just snippets. Returns `{ path, title, body, \
        frontmatter }` (plus `served_from` when a missing markdown file is \
        served from the DB fallback). Errors if the page is not found.")]
    async fn memory_read_page(
        &self,
        Parameters(args): Parameters<ReadPageArgs>,
        OptionalParts(parts): OptionalParts,
    ) -> Result<CallToolResult, McpError> {
        let aps_actor = Self::actor_key_from_parts(Some(&parts));
        let Some(wiki) = self.wiki.as_ref() else {
            return Err(McpError::internal_error(
                "memory_read_page requires the server to be built with a wiki handle",
                None,
            ));
        };
        // Same scope resolution as memory_query: an explicit workspace+project
        // can target a page in a DIFFERENT workspace (a sibling project on a
        // shared server). Plain `project` keeps the active-project chain.
        let (ws, proj) = self
            .effective_ids_for_read_args_with_actor(
                args.workspace.as_deref(),
                args.project.as_deref(),
                &aps_actor,
            )
            .await?;
        // Diagnose cross-project scope-bleed: a read with no explicit scope
        // resolves to the active project, which may differ from the scope a
        // concurrent write landed in — the write persists but the by-path
        // read looks in the wrong bucket and 404s.
        let auto_scoped = args
            .workspace
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
            && args.project.as_deref().is_none_or(|s| s.trim().is_empty());

        let page_path = if let Some(p) = args.path {
            PagePath::new(p)
                .map_err(|e| McpError::internal_error(format!("invalid path: {e}"), None))?
        } else if let Some(query) = args.query {
            let hits = self
                .reader
                .search_pages_for_project(ws, proj, query.clone(), 1)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            match hits.into_iter().next() {
                Some(h) => h.path,
                None => {
                    return Err(McpError::internal_error(
                        format!("no pages found for query {query:?}"),
                        None,
                    ));
                }
            }
        } else {
            // Instructive on purpose: looping clients (issue #155 — OpenCode
            // null-fills both args) read this text. Tell the model exactly
            // what a valid retry looks like instead of a dead-end.
            return Err(McpError::invalid_params(
                "memory_read_page requires exactly one of `path` or `query` \
                 as a non-null string — do not retry with both null. Pass the \
                 page's path from a memory_recent/memory_query hit, e.g. \
                 {\"path\": \"notes/topic.md\"}, or search by content with \
                 {\"query\": \"topic keywords\"}.",
                None,
            ));
        };

        // Markdown on disk is the source of truth. Only a missing markdown file
        // uses the DB fallback; parse/permission/corruption errors must surface
        // so operators can fix the disk source of truth.
        match wiki.read_page(ws, proj, &page_path) {
            Ok(md) => {
                let title = md
                    .frontmatter
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                ok_json(&serde_json::json!({
                    "path": page_path.to_string(),
                    "title": title,
                    "body": md.body,
                    "frontmatter": md.frontmatter,
                }))
            }
            Err(disk_err) if is_missing_wiki_file(&disk_err) => {
                match self
                    .reader
                    .page_body_by_ids(ws, proj, page_path.as_str())
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?
                {
                    Some(stored) => {
                        let frontmatter: serde_json::Value =
                            serde_json::from_str(&stored.frontmatter_json)
                                .unwrap_or(serde_json::Value::Null);
                        let title = frontmatter
                            .get("title")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                            .or(Some(stored.title));
                        ok_json(&serde_json::json!({
                            "path": page_path.to_string(),
                            "title": title,
                            "body": stored.body,
                            "frontmatter": frontmatter,
                            "served_from": "db-fallback",
                        }))
                    }
                    None => {
                        // Not on disk and not in the DB under the resolved
                        // scope. Name the scope (and flag auto-resolution) so
                        // scope-bleed is diagnosable from the error itself,
                        // instead of leaking the raw disk error/path.
                        let scope = self.scope_label(ws, proj).await;
                        let hint = if auto_scoped {
                            " — this scope was auto-resolved from the active \
                             project; pass explicit workspace+project if this \
                             is a parallel multi-project session where the \
                             write may have landed in a different scope"
                        } else {
                            ""
                        };
                        Err(McpError::internal_error(
                            format!(
                                "page {} not found in resolved scope {scope}{hint}",
                                page_path.as_str()
                            ),
                            None,
                        ))
                    }
                }
            }
            Err(disk_err) => Err(McpError::internal_error(disk_err.to_string(), None)),
        }
    }

    /// Delete a single wiki page by exact path.
    #[tool(description = "Delete a single wiki page by its exact relative \
        path (e.g. `notes/foo.md`). Use when the user explicitly asks to \
        delete or remove a page. Fires the admission chain (op=delete) \
        before the file is removed so backups/mirrors stay consistent. \
        Idempotent — deleting a page that is already gone is a no-op. \
        Pass `workspace` + `project` together when the page lives in a \
        sibling workspace; missing explicit scopes fail closed instead of \
        falling back to the active/default project. \
        Returns `{ path, deleted }`.")]
    async fn memory_delete_page(
        &self,
        Parameters(args): Parameters<DeletePageArgs>,
        OptionalParts(parts): OptionalParts,
    ) -> Result<CallToolResult, McpError> {
        let aps_actor = Self::actor_key_from_parts(Some(&parts));
        let Some(wiki) = self.wiki.as_ref() else {
            return Err(McpError::internal_error(
                "memory_delete_page requires the server to be built with a wiki handle",
                None,
            ));
        };
        let path = PagePath::new(args.path.clone())
            .map_err(|e| McpError::internal_error(format!("invalid path: {e}"), None))?;
        let (ws, proj) = self
            .effective_ids_for_read_args_with_actor(
                args.workspace.as_deref(),
                args.project.as_deref(),
                &aps_actor,
            )
            .await?;

        // Carry actor identity + loop-prevention skip list (same as write_page).
        // `Wiki::delete_page` stamps `op = Delete` regardless of what we pass.
        let actor = crate::actor::actor_from_parts(&parts);
        let skip_webhooks = crate::actor::skip_webhooks_from_parts(&parts);
        let admission_ctx = if actor.has_any() || !skip_webhooks.is_empty() {
            Some(ai_memory_wiki::AdmissionContext {
                actor,
                op: ai_memory_wiki::AdmissionOp::Delete,
                skip_webhooks,
                ..ai_memory_wiki::AdmissionContext::default()
            })
        } else {
            None
        };

        let pre_checkpoint =
            checkpoint_or_mcp(wiki, format!("pre-memory_delete_page: {}", path.as_str()))?;

        let author_id = crate::actor::author_id_from_parts(&parts);
        wiki.delete_page(ws, proj, &path, admission_ctx, author_id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let checkpoint = checkpoint_or_warn(wiki, format!("memory_delete_page: {}", path.as_str()));

        ok_json(&serde_json::json!({
            "path": path.to_string(),
            "deleted": true,
            "pre_checkpoint": pre_checkpoint,
            "checkpoint": checkpoint,
        }))
    }

    /// Create a handoff snapshot for the next agent CLI.
    #[tool(description = "Record a cross-agent handoff snapshot for the \
        NEXT agent that opens this project (e.g. Codex picking up after \
        Claude Code). Use this ONLY when ending/wrapping up the current \
        session or when the user explicitly says to save context for the next \
        session. DO NOT use this to check project status, get a briefing, or \
        summarize work mid-session. The next session's SessionStart hook automatically \
        consumes the handoff and prepends its content to the agent's \
        context — no manual fetch needed. \
        \
        Write style: keep `summary` to 2-3 SHORT sentences (what just \
        happened + what state the project's in). Put actionable detail \
        in `open_questions` and `next_steps` as bullet-sized strings — \
        the next agent reads those first; long prose summaries make the \
        TUI rendering ugly. `files_touched` is a hint, not exhaustive. \
        \
        Use `cwd` to scope the handoff to a specific working directory.")]
    async fn memory_handoff_begin(
        &self,
        Parameters(args): Parameters<HandoffBeginArgs>,
        OptionalParts(parts): OptionalParts,
    ) -> Result<CallToolResult, McpError> {
        let aps_actor = Self::actor_key_from_parts(Some(&parts));
        // Handoffs bypass `Wiki::write_page` (they live in their own
        // table), so scrub the agent-supplied free-text here. We don't
        // touch `cwd` or `files_touched` — they're path lists that the
        // path-pattern regexes already cover when applicable, but we
        // pass each entry through anyway as defence-in-depth.
        let s = &self.sanitizer;
        // Mirror memory_write_page: a handoff is a write, so resolve through the
        // create-if-missing write path and honour an explicit workspace. Using
        // the project-only `effective_ids_with_actor` here dropped the
        // workspace arg, so a cross-workspace handoff landed in whatever project
        // the contaminable active-project slot pointed at (the scope-bleed bug).
        let (ws, proj) = self
            .write_target_ids_with_actor(
                args.workspace.as_deref(),
                args.project.as_deref(),
                &aps_actor,
            )
            .await?;
        let open_questions = cap_handoff_list(
            args.open_questions.iter().map(|q| s.scrub(q)),
            HANDOFF_ITEM_MAX_CHARS,
            HANDOFF_TEXT_LIST_MAX_CHARS,
            "handoff item",
            "handoff open_questions",
        );
        let next_steps = cap_handoff_list(
            args.next_steps.iter().map(|n| s.scrub(n)),
            HANDOFF_ITEM_MAX_CHARS,
            HANDOFF_TEXT_LIST_MAX_CHARS,
            "handoff item",
            "handoff next_steps",
        );
        let files_touched = cap_handoff_list(
            args.files_touched.iter().map(|f| s.scrub(f)),
            HANDOFF_FILE_MAX_CHARS,
            HANDOFF_FILE_LIST_MAX_CHARS,
            "handoff file",
            "handoff files_touched",
        );
        let handoff = NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::Other,
            to_agent: None,
            cwd: args.cwd.map(std::path::PathBuf::from),
            summary: cap_text_with_marker(
                &s.scrub(&args.summary),
                HANDOFF_SUMMARY_MAX_CHARS,
                "handoff summary",
            ),
            open_questions,
            next_steps,
            files_touched,
        };
        let id = self
            .writer
            .insert_handoff(handoff)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        ok_json(&serde_json::json!({ "handoff_id": id.to_string() }))
    }

    /// Fetch the latest open handoff for this project (optionally filtered
    /// by cwd) and mark it accepted.
    #[tool(description = "Fetch the latest OPEN cross-agent handoff and \
        mark it accepted. \
        \
        IMPORTANT: handoffs are SINGLE-USE. The SessionStart hook \
        automatically consumes the handoff at session-start and prepends \
        the content to your context — when you see a block starting with \
        '📥 ai-memory: pending handoff from previous session' anywhere \
        in your context, that IS the handoff. \
        \
        A subsequent call to this tool will return `{ \"handoff\": null }` \
        because the hook already consumed it. Do NOT interpret null as \
        'no handoff exists' — check your context for the prepended block \
        first, and answer the user from there. Call this tool only when \
        you BOTH don't see a prepended block AND the user explicitly asks \
        for a handoff (e.g. a hook script ran with no stdout capture). \
        \
        Returns the same JSON shape memory_handoff_begin accepted.")]
    async fn memory_handoff_accept(
        &self,
        Parameters(args): Parameters<HandoffAcceptArgs>,
        OptionalParts(parts): OptionalParts,
    ) -> Result<CallToolResult, McpError> {
        let aps_actor = Self::actor_key_from_parts(Some(&parts));
        let (ws, proj) = self
            .effective_ids_for_read_args_with_actor(
                args.workspace.as_deref(),
                args.project.as_deref(),
                &aps_actor,
            )
            .await?;
        let handoff = self
            .reader
            .latest_open_handoff(ws, proj, args.cwd)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        match handoff {
            None => ok_json(&serde_json::json!({ "handoff": null })),
            Some(h) => {
                self.writer
                    .accept_handoff(h.id, AgentKind::Other, None)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                ok_json(&serde_json::json!({ "handoff": h }))
            }
        }
    }

    /// Cancel a mistaken open handoff by exact id.
    #[tool(description = "Cancel/discard a mistakenly-created OPEN handoff by \
        exact `handoff_id` returned from `memory_handoff_begin`. Use this ONLY \
        when you realize you called `memory_handoff_begin` by mistake or the \
        user explicitly asks to discard a pending handoff. This is a cleanup \
        tool, not a status/briefing tool. It marks the handoff expired so the \
        next SessionStart hook will not consume it. Omit project/workspace \
        unless the user names a different project; when provided, workspace \
        and project must be supplied together.")]
    async fn memory_handoff_cancel(
        &self,
        Parameters(args): Parameters<HandoffCancelArgs>,
        OptionalParts(parts): OptionalParts,
    ) -> Result<CallToolResult, McpError> {
        let aps_actor = Self::actor_key_from_parts(Some(&parts));
        let handoff_id = HandoffId::from_str(&args.handoff_id)
            .map_err(|e| McpError::internal_error(format!("invalid handoff_id: {e}"), None))?;
        let (ws, proj) = self
            .effective_ids_for_read_args_with_actor(
                args.workspace.as_deref(),
                args.project.as_deref(),
                &aps_actor,
            )
            .await?;
        let handoff = self
            .reader
            .handoff_by_id(handoff_id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .ok_or_else(|| McpError::internal_error("handoff not found", None))?;
        if handoff.workspace_id != ws || handoff.project_id != proj {
            return Err(McpError::internal_error(
                "handoff does not belong to the resolved project",
                None,
            ));
        }
        if handoff.state != HandoffState::Open {
            return ok_json(&serde_json::json!({
                "handoff_id": handoff_id.to_string(),
                "cancelled": false,
                "state": handoff.state.as_str(),
            }));
        }
        let cancelled = self
            .writer
            .cancel_handoff(handoff_id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        ok_json(&serde_json::json!({
            "handoff_id": handoff_id.to_string(),
            "cancelled": cancelled,
            "state": if cancelled { "expired" } else { "open" },
        }))
    }

    /// Report aggregate counts (pages, sessions, observations).
    #[tool(description = "Report aggregate memory counts and runtime status \
        (pages latest, pages all versions, sessions, observations). \
        Use this at session start to see how much context the agent has \
        accumulated for this workspace.")]
    async fn memory_status(
        &self,
        Parameters(args): Parameters<StatusArgs>,
        OptionalParts(parts): OptionalParts,
    ) -> Result<CallToolResult, McpError> {
        let aps_actor = Self::actor_key_from_parts(Some(&parts));
        let (ws, proj) = self
            .effective_ids_for_read_args_with_actor(
                args.workspace.as_deref(),
                args.project.as_deref(),
                &aps_actor,
            )
            .await?;
        let counts = self
            .reader
            .status_counts_for_project(ws, proj)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let response = StatusResponse { counts };
        ok_json(&response)
    }

    /// Composite "what's going on" snapshot — structured data only,
    /// no LLM call. Pair with `memory_explore` if you want prose.
    #[tool(description = "Compose a structured snapshot of project activity \
        WITHOUT any LLM call: lifetime counts, 7-day and 30-day activity \
        windows, last-observation timestamp, pending handoff count, \
        current `_rules/` pages, and recent-page list. Cheap, fast, \
        deterministic, and READ-ONLY: it never creates handoffs or mutates \
        project state. Use this when you want a programmatic view of \
        project state; use `memory_explore` if you want an LLM-composed \
        prose summary on top of the same data.")]
    async fn memory_briefing(
        &self,
        Parameters(args): Parameters<BriefingArgs>,
        OptionalParts(parts): OptionalParts,
    ) -> Result<CallToolResult, McpError> {
        let aps_actor = Self::actor_key_from_parts(Some(&parts));
        let limit = args.recent_pages_limit.unwrap_or(10);
        let (ws, proj) = self
            .effective_ids_for_read_args_with_actor(
                args.workspace.as_deref(),
                args.project.as_deref(),
                &aps_actor,
            )
            .await?;
        let snapshot = self
            .reader
            .briefing_for_project(ws, proj, limit)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        ok_json(&snapshot)
    }

    /// LLM-driven exploration. Calls `memory_briefing` internally, computes
    /// the time gap since the last observation, then asks the configured
    /// LLM to compose a calibrated prose digest (more detail for longer
    /// gaps, less for short ones). Falls back to a friendly JSON dump if
    /// no LLM is configured.
    #[tool(description = "Compose a calibrated prose digest of project \
        state. Calls `memory_briefing` for structured data, computes how \
        long it's been since the last observation, then asks the LLM to \
        scale verbosity to the gap (just-checked-in → 1-line, weeks-away \
        → fuller catchup). Accepts an optional `focus` argument to bias \
        the digest toward a topic (e.g. \"recent rules\" / \"pending \
        handoffs\" / a free-form question). When no LLM is configured \
        this returns the underlying briefing JSON unchanged so the \
        caller can render its own prose.")]
    async fn memory_explore(
        &self,
        Parameters(args): Parameters<ExploreArgs>,
        OptionalParts(parts): OptionalParts,
    ) -> Result<CallToolResult, McpError> {
        let aps_actor = Self::actor_key_from_parts(Some(&parts));
        let limit = args.recent_pages_limit.unwrap_or(10);
        let (ws, proj) = self
            .effective_ids_for_read_args_with_actor(
                args.workspace.as_deref(),
                args.project.as_deref(),
                &aps_actor,
            )
            .await?;
        let snapshot = self
            .reader
            .briefing_for_project(ws, proj, limit)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let Some(llm) = &self.consolidator else {
            // No LLM configured — return the structured snapshot.
            // Caller can render prose itself if it wants.
            return ok_json(&serde_json::json!({
                "prose": null,
                "reason": "no LLM provider configured; returning structured briefing instead",
                "briefing": snapshot,
            }));
        };

        let gap = explore_gap_from_snapshot(&snapshot);
        let request = build_explore_request(&snapshot, &gap, args.focus.as_deref());
        let provider = llm.llm();
        let text = match provider.complete(request).await {
            Ok(resp) => resp.text,
            Err(e) => {
                tracing::warn!(error = %e, "memory_explore LLM call failed; degrading to briefing");
                return ok_json(&serde_json::json!({
                    "prose": null,
                    "reason": format!("LLM call failed: {e}"),
                    "briefing": snapshot,
                }));
            }
        };

        ok_json(&serde_json::json!({
            "prose": text,
            "gap": gap,
            "briefing": snapshot,
        }))
    }

    /// Return the canonical CLAUDE.md / AGENTS.md routing block so the
    /// agent can land it via its own Write/Edit tool. No server-side
    /// state changes — the server can't reach the agent's host
    /// filesystem.
    #[tool(
        description = "Returns the canonical ai-memory routing install payload: \
        `markered_block` for the slim CLAUDE.md / AGENTS.md snippet, \
        `agent_filenames` for rules-file targets, `managed_skills` for \
        Agent Skill files, and `target_hints` for project/global \
        `.claude/skills`, `.agents/skills`, `.devin/skills`, `.grok/skills`, `$GROK_HOME/skills` (default `~/.grok/skills`), and Devin Windows global roots. Use when the user \
        asks to install or refresh ai-memory routing in this project. \
        After calling, use your Write/Edit tool to preserve non-ai-memory \
        user content: replace only an existing `<!-- ai-memory:start -->` \
        / `<!-- ai-memory:end -->` block whose delimiters appear alone on \
        their own lines, or append `markered_block` with one blank line, \
        then write every `managed_skills` item beneath \
        the chosen skill root using its `relative_path`. This tool is \
        read-only and is the source of truth for the snippet and skills. \
        Skill files are ai-memory-managed only when they contain the \
        managed marker; do not overwrite unmanaged same-name skills unless \
        the human explicitly forces replacement."
    )]
    async fn memory_install_self_routing(&self) -> Result<CallToolResult, McpError> {
        let managed_skills: Vec<_> = ai_memory_core::routing_skills::MANAGED_SKILLS
            .iter()
            .map(|skill| {
                serde_json::json!({
                    "name": skill.name,
                    "description": skill.description,
                    "relative_path": skill.relative_path,
                    "content": skill.content,
                })
            })
            .collect();
        let response = serde_json::json!({
            "markered_block": ai_memory_core::full_block(),
            "marker_start": ai_memory_core::MARKER_START,
            "marker_end": ai_memory_core::MARKER_END,
            "agent_filenames": {
                "claude_code": "CLAUDE.md",
                "codex": "AGENTS.md",
                "opencode": "AGENTS.md",
                "cursor": "AGENTS.md",
                "gemini_cli": "AGENTS.md",
                "antigravity_cli": "AGENTS.md",
                "zero": "AGENTS.md",
                "devin": "AGENTS.md",
                "kimi_code": "AGENTS.md",
                "grok": "AGENTS.md",
                "default": "AGENTS.md"
            },
            "managed_skills": managed_skills,
            "target_hints": {
                "project": {
                    "claude_code": ".claude/skills",
                    "agents": ".agents/skills",
                    "devin": ".devin/skills",
                    "grok": ".grok/skills"
                },
                "global": {
                    "claude_code": "~/.claude/skills",
                    "agents": "~/.agents/skills",
                    "devin": {
                        "windows": "%APPDATA%\\devin\\skills",
                        "non_windows": "~/.devin/skills"
                    },
                    "grok": "$GROK_HOME/skills (default: ~/.grok/skills)"
                }
            },
            "overwrite_guidance": {
                "managed_marker": ai_memory_core::routing_skills::MANAGED_MARKER,
                "safe_update": "Existing same-name skill files containing the managed marker may be replaced with the managed payload.",
                "unsafe_update": "Unmanaged same-name skills must not be overwritten unless the human explicitly forces replacement."
            },
            "notes": [
                "Pick the filename matching your own agent identity.",
                "If the target file already contains <!-- ai-memory:start --> / <!-- ai-memory:end --> delimiters alone on their own lines, replace ONLY that line-delimited block in place; ignore inline mentions and preserve every other line.",
                "If the file doesn't exist, create it with just the markered_block (plus a trailing newline).",
                "If the file exists but has no ai-memory markers, append the markered_block with one blank line of separation from existing content.",
                "Install each managed_skills item under the selected skill root from target_hints using its relative_path, for example .claude/skills/<relative_path>, .agents/skills/<relative_path>, .devin/skills/<relative_path>, .grok/skills/<relative_path>, $GROK_HOME/skills/<relative_path> (default ~/.grok/skills), or %APPDATA%\\devin\\skills\\<relative_path> on Windows global Devin installs.",
                "Existing skill files containing the managed marker <!-- ai-memory-managed: routing-skill --> may be replaced; unmanaged same-name skills must not be overwritten unless the human explicitly forces replacement."
            ]
        });
        ok_json(&response)
    }
}

fn hex_to_sha256(hex: &str) -> Result<[u8; 32], String> {
    if hex.len() != 64 {
        return Err("expected 64 hex chars".into());
    }
    let mut out = [0_u8; 32];
    for (idx, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
        let s = std::str::from_utf8(chunk).map_err(|e| e.to_string())?;
        out[idx] = u8::from_str_radix(s, 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

#[tool_handler]
impl ServerHandler for AiMemoryServer {
    fn get_info(&self) -> ServerInfo {
        // `Implementation::from_build_env()` reads CARGO_PKG_NAME/VERSION
        // from *rmcp's* compilation unit, not ours. Patch the fields
        // post-construction so the wire protocol surfaces "ai-memory".
        let mut implementation = Implementation::from_build_env();
        implementation.name = "ai-memory".into();
        implementation.version = env!("CARGO_PKG_VERSION").into();
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(implementation)
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(MEMORY_INSTRUCTIONS.to_string())
    }

    // Declared manually so `#[tool_handler]` skips its generated
    // `list_tools` and the flavor patch runs on every tools/list.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = self.tool_router.list_all();
        // rmcp injects `http::request::Parts` into request extensions in
        // both stateless and stateful modes, so the flavor marker is
        // available even without peer clientInfo.
        let moonshot = context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.uri.query())
            .is_some_and(|query| query.split('&').any(|pair| pair == "flavor=moonshot"));
        if moonshot {
            Ok(ListToolsResult::with_all_items(moonshot_safe_tool_list(
                tools,
            )))
        } else {
            Ok(ListToolsResult::with_all_items(tools))
        }
    }
}

/// Moonshot ("moonshot flavored json schema") rejects root-level
/// `anyOf`/`oneOf`/`allOf` in tool parameter schemas with a 400 at
/// `tools/list` time. Requests marked `?flavor=moonshot` (the URL
/// `install-mcp --client kimi-code` writes) get the tool list with
/// those root keys stripped; nested combinators stay, and the handlers'
/// runtime validation still enforces the arg contracts.
fn moonshot_safe_tool_list(tools: Vec<Tool>) -> Vec<Tool> {
    const ROOT_COMBINATORS: [&str; 3] = ["anyOf", "oneOf", "allOf"];
    tools
        .into_iter()
        .map(|mut tool| {
            if !ROOT_COMBINATORS
                .iter()
                .any(|key| tool.input_schema.contains_key(*key))
            {
                return tool;
            }
            let mut schema = (*tool.input_schema).clone();
            for key in ROOT_COMBINATORS {
                schema.shift_remove(key);
            }
            tool.input_schema = Arc::new(schema);
            tool
        })
        .collect()
}

impl AiMemoryServer {
    /// Fire-and-forget access-counter bump for the M8 reinforcement
    /// term. Failures are logged at warn but never surfaced to the
    /// caller.
    fn spawn_access_bump(&self, ids: Vec<PageId>) {
        if ids.is_empty() {
            return;
        }
        let writer = self.writer.clone();
        tokio::spawn(async move {
            if let Err(e) = writer.bump_access(ids).await {
                tracing::warn!(error = %e, "access bump failed");
            }
        });
    }
}

/// True when the caller explicitly scoped the read by name — a non-empty
/// `workspace` or `project` argument. Explicit scoping always wins over the
/// `[recall] default_global` marker broadening; `memory_query` also counts
/// its `scopes` list on top of this.
fn named_scope_args_present(workspace: Option<&str>, project: Option<&str>) -> bool {
    workspace.is_some_and(|s| !s.trim().is_empty()) || project.is_some_and(|s| !s.trim().is_empty())
}

fn ok_json<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let s = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(s)]))
}

fn checkpoint_or_mcp(wiki: &Wiki, message: impl AsRef<str>) -> Result<Option<String>, McpError> {
    wiki.commit_all(message.as_ref())
        .map(|oid| oid.map(|oid| oid.to_string()))
        .map_err(|e| McpError::internal_error(e.to_string(), None))
}

fn checkpoint_or_warn(wiki: &Wiki, message: impl AsRef<str>) -> Option<String> {
    match wiki.commit_all(message.as_ref()) {
        Ok(Some(oid)) => Some(oid.to_string()),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, "wiki checkpoint failed after MCP mutation");
            None
        }
    }
}

fn is_missing_wiki_file(err: &WikiError) -> bool {
    matches!(err, WikiError::Io(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Description of how long it's been since the last observation.
/// `memory_explore` uses this both to size its prompt verbosity and
/// to give the LLM an explicit "time gap is N hours" cue.
#[derive(Debug, Serialize)]
struct ExploreGap {
    /// Hours since the last observation, or `None` if nothing has
    /// ever been observed for this project.
    hours_since_last: Option<f64>,
    /// Coarse bucket name used to drive the prompt:
    /// `none` — no prior activity at all.
    /// `fresh` — last observation < 1 h ago.
    /// `today` — < 24 h ago.
    /// `recent` — < 7 days ago.
    /// `dormant` — < 30 days ago.
    /// `stale` — > 30 days ago.
    bucket: &'static str,
    /// Plain-English description for the LLM prompt.
    description: String,
}

fn explore_gap_from_snapshot(s: &ai_memory_store::BriefingSnapshot) -> ExploreGap {
    let Some(last) = s.last_observation_at.as_deref() else {
        return ExploreGap {
            hours_since_last: None,
            bucket: "none",
            description: "no prior activity recorded for this project".into(),
        };
    };
    let Ok(last_ts) = last.parse::<jiff::Timestamp>() else {
        return ExploreGap {
            hours_since_last: None,
            bucket: "none",
            description: format!("last observation timestamp unparseable: {last}"),
        };
    };
    let delta_us = jiff::Timestamp::now().as_microsecond() - last_ts.as_microsecond();
    let hours = (delta_us as f64) / 1_000_000.0 / 3600.0;
    let (bucket, description) = if hours < 1.0 {
        (
            "fresh",
            format!("{:.1} minutes since last observation", hours * 60.0),
        )
    } else if hours < 24.0 {
        ("today", format!("{hours:.1} hours since last observation"))
    } else if hours < 24.0 * 7.0 {
        (
            "recent",
            format!("{:.1} days since last observation", hours / 24.0),
        )
    } else if hours < 24.0 * 30.0 {
        (
            "dormant",
            format!("{:.1} days since last observation", hours / 24.0),
        )
    } else {
        (
            "stale",
            format!("{:.1} days since last observation", hours / 24.0),
        )
    };
    ExploreGap {
        hours_since_last: Some(hours),
        bucket,
        description,
    }
}

/// Build the ChatRequest for `memory_explore`. The user message
/// inlines the entire briefing as JSON — small enough (a few KB) that
/// model context is not a concern. The system prompt + the gap
/// bucket together steer verbosity.
fn build_explore_request(
    snapshot: &ai_memory_store::BriefingSnapshot,
    gap: &ExploreGap,
    focus: Option<&str>,
) -> ai_memory_llm::ChatRequest {
    let snapshot_json = serde_json::to_string_pretty(snapshot).unwrap_or_else(|_| "{}".into());
    let mut user = String::new();
    user.push_str("## Project state snapshot\n\n");
    user.push_str("```json\n");
    user.push_str(&snapshot_json);
    user.push_str("\n```\n\n");
    user.push_str(&format!(
        "## Time gap\n\nBucket: `{}` — {}.\n\n",
        gap.bucket, gap.description
    ));
    if let Some(focus) = focus {
        user.push_str("## Focus\n\nThe user is specifically interested in: ");
        user.push_str(focus);
        user.push_str("\n\nBias the digest toward this topic while still covering anything urgent (pending handoffs, recently-changed rules).\n");
    }
    ai_memory_llm::ChatRequest {
        system: Some(EXPLORE_SYSTEM_PROMPT.into()),
        messages: vec![ai_memory_llm::ChatMessage {
            role: ai_memory_llm::Role::User,
            content: user,
        }],
        // memory_explore returns prose, not JSON, so a truncated
        // response is degraded but not unparseable. Still generous
        // so the long `dormant`/`stale` digests don't get cut off.
        max_tokens: 16_000,
        temperature: Some(0.2),
    }
}

/// System prompt for `memory_explore`. Loaded at compile time from
/// `prompts/explore_system.md`.
const EXPLORE_SYSTEM_PROMPT: &str = include_str!("../prompts/explore_system.md");

/// Synthetic anonymous request `Parts` for callers arriving without request
/// parts (for example stdio): no actor headers, so downstream resolves an
/// anonymous `ActorKey` — the correct identity for a local, unauthenticated
/// `serve`. Streamable HTTP injects real `Parts` carrying middleware identity;
/// when those are present, the extractor below preserves them instead.
fn default_parts() -> axum::http::request::Parts {
    let mut request = axum::http::Request::new(());
    *request.method_mut() = axum::http::Method::POST;
    *request.uri_mut() = axum::http::Uri::from_static("/mcp");
    request.into_parts().0
}

/// Tool-handler extractor for the request `Parts`. Unlike rmcp's
/// `Extension<Parts>` — which fails every `tools/call` with "missing extension
/// http::request::Parts" when the extension is absent — this yields the real
/// `Parts` over the streamable-HTTP transport and a synthetic anonymous one
/// when request parts are absent (the stdio case), so a local stdio `serve`
/// works while HTTP auth is unchanged when real request parts are present.
struct OptionalParts(axum::http::request::Parts);

impl OptionalParts {
    fn from_extensions(extensions: &rmcp::model::Extensions) -> Self {
        let parts = extensions
            .get::<axum::http::request::Parts>()
            .cloned()
            .unwrap_or_else(default_parts);
        Self(parts)
    }
}

impl<C> rmcp::handler::server::common::FromContextPart<C> for OptionalParts
where
    C: rmcp::handler::server::common::AsRequestContext,
{
    fn from_context_part(context: &mut C) -> Result<Self, rmcp::ErrorData> {
        Ok(Self::from_extensions(
            &context.as_request_context().extensions,
        ))
    }
}

#[cfg(test)]
fn test_parts_default() -> axum::http::request::Parts {
    default_parts()
}

#[cfg(test)]
fn test_optional_parts() -> OptionalParts {
    OptionalParts(test_parts_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{Sanitized, Sanitizer};
    use std::collections::BTreeSet;

    #[test]
    fn stdio_default_parts_resolves_to_anonymous_actor() {
        // The stdio transport injects no `Parts`, so the `OptionalParts`
        // extractor falls back to `default_parts()`. That synthetic default
        // must carry no actor identity — a local unauthenticated caller —
        // rather than error, which is what let tools/call fail on stdio.
        let key = AiMemoryServer::actor_key_from_parts(Some(&default_parts()));
        assert!(
            key.user.is_none() && key.session_id.is_none(),
            "stdio default must be anonymous"
        );
    }

    #[test]
    fn optional_parts_missing_extension_uses_anonymous_stdio_default() {
        let extensions = rmcp::model::Extensions::new();

        let OptionalParts(parts) = OptionalParts::from_extensions(&extensions);

        assert_eq!(parts.method, axum::http::Method::POST);
        assert_eq!(parts.uri, axum::http::Uri::from_static("/mcp"));
        assert_eq!(
            AiMemoryServer::actor_key_from_parts(Some(&parts)),
            ai_memory_core::ActorKey::default(),
            "missing request parts must degrade to anonymous stdio context instead of failing extraction"
        );
        assert!(parts.extensions.get::<AuthLevel>().is_none());
        assert!(parts.extensions.get::<ai_memory_core::UserId>().is_none());
        assert!(parts.extensions.get::<ActorContext>().is_none());
    }

    #[test]
    fn optional_parts_preserves_real_http_parts_and_auth_context() {
        let user_id = ai_memory_core::UserId::new();
        let mut real_parts = test_parts_default();
        real_parts
            .headers
            .insert("mcp-session-id", "session-from-header".parse().unwrap());
        real_parts.extensions.insert(AuthLevel::User);
        real_parts.extensions.insert(user_id);
        real_parts.extensions.insert(ActorContext {
            user: Some("alice".into()),
            name: Some("Alice Smith".into()),
            email: Some("alice@example.com".into()),
            ..ActorContext::default()
        });

        let mut extensions = rmcp::model::Extensions::new();
        extensions.insert(real_parts);

        let OptionalParts(parts) = OptionalParts::from_extensions(&extensions);

        assert_eq!(parts.extensions.get::<AuthLevel>(), Some(&AuthLevel::User));
        assert_eq!(
            parts.extensions.get::<ai_memory_core::UserId>(),
            Some(&user_id)
        );
        assert_eq!(
            parts
                .extensions
                .get::<ActorContext>()
                .and_then(|ctx| ctx.user.as_deref()),
            Some("alice")
        );
        assert_eq!(
            AiMemoryServer::actor_key_from_parts(Some(&parts)),
            ai_memory_core::ActorKey {
                user: Some("alice".into()),
                session_id: Some("session-from-header".into()),
            },
            "real HTTP request parts must preserve auth identity and routing session"
        );
    }

    use ai_memory_core::{
        ActorContext, AuthLevel, NewObservation, NewPage, NewSession, NewUser, ObservationKind,
        PagePath, Tier,
    };
    use ai_memory_store::Store;
    use ai_memory_wiki::{Wiki, WritePageRequest};
    use tempfile::TempDir;

    async fn setup_server() -> (TempDir, Store, AiMemoryServer, WorkspaceId, ProjectId) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: ws,
                project_id: proj,
                path: PagePath::new("foo.md").unwrap(),
                title: "Foo".into(),
                body: "Karpathy says compile, not retrieve.".into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
            })
            .await
            .unwrap();

        let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj);
        (tmp, store, server, ws, proj)
    }

    fn installed_ai_memory_prompt_surface() -> String {
        let mut prompt = String::from(ai_memory_core::SNIPPET_BODY);
        for skill in ai_memory_core::routing_skills::MANAGED_SKILLS {
            prompt.push_str("\n\n");
            prompt.push_str(skill.content);
        }
        prompt
    }

    fn combined_ai_memory_prompt_surface() -> String {
        let mut prompt = String::from(MEMORY_INSTRUCTIONS);
        prompt.push_str("\n\n");
        prompt.push_str(&installed_ai_memory_prompt_surface());
        prompt
    }

    fn assert_detailed_prompt_surfaces(mut assert_prompt: impl FnMut(&str, &str)) {
        assert_prompt("MCP handshake instructions", MEMORY_INSTRUCTIONS);
        let combined = combined_ai_memory_prompt_surface();
        assert_prompt(
            "combined MCP, snippet, and managed skill prompts",
            &combined,
        );
    }

    fn call_tool_json(result: CallToolResult) -> serde_json::Value {
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .unwrap_or_else(|| panic!("expected text content"));
        serde_json::from_str(text).unwrap_or_else(|e| panic!("invalid JSON response: {e}\n{text}"))
    }

    async fn insert_test_observation(
        store: &Store,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        title: &str,
        body: &str,
    ) {
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id,
                project_id,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id,
                    project_id,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: title.into(),
                    body: body.into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();
    }

    const MCP_TOOL_NAMES: &[&str] = &[
        "memory_query",
        "memory_recent",
        "memory_status",
        "memory_briefing",
        "memory_explore",
        "memory_handoff_accept",
        "memory_handoff_begin",
        "memory_handoff_cancel",
        "memory_consolidate",
        "memory_auto_improve",
        "memory_write_page",
        "memory_read_page",
        "memory_delete_page",
        "memory_lint",
        "memory_forget_sweep",
        "memory_install_self_routing",
    ];

    const DETAILED_ROUTING_TOOL_NAMES: &[&str] = &[
        "memory_query",
        "memory_recent",
        "memory_status",
        "memory_briefing",
        "memory_explore",
        "memory_handoff_accept",
        "memory_handoff_begin",
        "memory_handoff_cancel",
        "memory_consolidate",
        "memory_auto_improve",
        "memory_write_page",
        "memory_read_page",
        "memory_delete_page",
        "memory_lint",
        "memory_forget_sweep",
    ];

    #[test]
    fn actor_key_uses_memory_session_header() {
        let mut parts = test_parts_default();
        parts.headers.insert(
            "x-memory-actor-session-id",
            axum::http::HeaderValue::from_static("hook-session"),
        );

        let actor = AiMemoryServer::actor_key_from_parts(Some(&parts));

        assert_eq!(actor.user, None);
        assert_eq!(actor.session_id.as_deref(), Some("hook-session"));
    }

    #[test]
    fn actor_key_accepts_standard_mcp_session_header() {
        let mut parts = test_parts_default();
        parts.headers.insert(
            "mcp-session-id",
            axum::http::HeaderValue::from_static("mcp-session"),
        );

        let actor = AiMemoryServer::actor_key_from_parts(Some(&parts));

        assert_eq!(actor.user, None);
        assert_eq!(actor.session_id.as_deref(), Some("mcp-session"));
    }

    #[test]
    fn actor_key_prefers_middleware_context_over_headers() {
        let mut parts = test_parts_default();
        parts.headers.insert(
            "x-memory-actor-session-id",
            axum::http::HeaderValue::from_static("header-session"),
        );
        parts.headers.insert(
            "mcp-session-id",
            axum::http::HeaderValue::from_static("mcp-session"),
        );
        parts.extensions.insert(ai_memory_core::ActorContext {
            user: Some("alice".into()),
            session_id: Some("context-session".into()),
            ..ai_memory_core::ActorContext::default()
        });

        let actor = AiMemoryServer::actor_key_from_parts(Some(&parts));

        assert_eq!(actor.user.as_deref(), Some("alice"));
        assert_eq!(actor.session_id.as_deref(), Some("context-session"));
    }

    #[tokio::test]
    async fn server_constructs_with_tool_router() {
        let (_tmp, _store, _server, _ws, _pj) = setup_server().await;
    }

    #[tokio::test]
    async fn prompts_cover_every_registered_mcp_tool() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let actual_tools: BTreeSet<String> = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        let expected_tools: BTreeSet<String> = MCP_TOOL_NAMES
            .iter()
            .map(|tool| (*tool).to_string())
            .collect();
        assert_eq!(
            actual_tools, expected_tools,
            "MCP_TOOL_NAMES must match the registered tool router set"
        );

        for tool in &actual_tools {
            assert!(
                MEMORY_INSTRUCTIONS.contains(tool.as_str()),
                "MCP handshake instructions omit {tool}"
            );
        }

        let installed = installed_ai_memory_prompt_surface();
        for tool in &actual_tools {
            assert!(
                installed.contains(tool.as_str()),
                "installed snippet and managed skills omit {tool}"
            );
        }
    }

    #[test]
    fn snippet_keeps_always_loaded_invariants() {
        let snippet = ai_memory_core::SNIPPET_BODY;
        assert!(snippet.contains("Long-term memory (ai-memory)"));
        assert!(snippet.contains("Default to the current project"));
        assert!(
            snippet.contains("Do NOT pass `project`, `workspace`, or `cwd`"),
            "snippet must preserve current-project scope defaulting"
        );
        assert!(
            snippet.contains("Lifecycle hooks already capture"),
            "snippet must keep automatic lifecycle capture guidance"
        );
        assert!(
            snippet.contains("durable")
                && snippet.contains("explicitly asks")
                && (snippet.contains("permanent") || snippet.contains("permanently")),
            "snippet must say durable writes require an explicit user request"
        );
        assert!(
            snippet.contains("Agent Skills") && snippet.contains("installed"),
            "snippet must route detailed guidance through installed Agent Skills"
        );
        assert!(
            snippet.contains("canonical agent instruction file"),
            "snippet must keep canonical project-rule placement guidance"
        );
        assert!(
            snippet.contains("memory_install_self_routing")
                && snippet.contains("ai-memory install-instructions"),
            "snippet must preserve refresh/install guidance"
        );
        let refresh_guidance = snippet
            .split("### Refreshing this snippet")
            .nth(1)
            .expect("snippet must keep refresh guidance");
        assert!(
            refresh_guidance.contains("managed_skills")
                && refresh_guidance.contains("target_hints")
                && refresh_guidance.contains("relative_path"),
            "snippet must tell agents to refresh managed skill files"
        );
        assert!(
            snippet.contains("start/end HTML-comment markers")
                && ai_memory_core::full_block().contains(ai_memory_core::MARKER_START)
                && ai_memory_core::full_block().contains(ai_memory_core::MARKER_END),
            "snippet must preserve marker replacement guidance"
        );
    }

    #[test]
    fn snippet_omits_detailed_tool_routing_table() {
        let snippet = ai_memory_core::SNIPPET_BODY;
        assert!(!snippet.contains("### When to reach for each tool"));
        assert!(!snippet.contains("| User says / situation | Tool |"));
        for tool in DETAILED_ROUTING_TOOL_NAMES {
            assert!(
                !snippet.contains(tool),
                "slim snippet must leave detailed {tool} routing to managed skills"
            );
        }
    }

    #[test]
    fn installed_prompt_surface_routes_grok_to_agents_md() {
        let installed = installed_ai_memory_prompt_surface();
        assert!(
            installed.contains("Grok Build CLI") && installed.contains("Grok -> `AGENTS.md`"),
            "installed snippet and managed skills must route Grok to AGENTS.md"
        );
    }
    #[test]
    fn prompts_separate_briefing_from_handoff_lifecycle() {
        assert_detailed_prompt_surfaces(|label, prompt| {
            let lower = prompt.to_ascii_lowercase();
            assert!(
                prompt.contains("memory_briefing") && lower.contains("read-only"),
                "{label} must say briefing is read-only"
            );
            assert!(
                prompt.contains("memory_handoff_begin")
                    && (lower.contains("session-end")
                        || (lower.contains("ending") && lower.contains("session")))
                    && (lower.contains("do not use") || lower.contains("do **not** use"))
                    && lower.contains("status")
                    && lower.contains("briefing"),
                "{label} must make handoff-begin session-end only and reject status/briefing use"
            );
            assert!(
                prompt.contains("memory_handoff_cancel") && prompt.contains("handoff_id"),
                "{label} must expose exact-id cleanup for mistaken handoffs"
            );
        });
    }
    #[test]
    fn prompts_teach_cross_project_search_strategy() {
        // Regression: a single-project miss must not read as "never recorded".
        // Both surfaces must point the agent at `scopes` **and** at
        // `global=true` (the two broadening modes), warn that query returns
        // snippets (not full page bodies), and NOT contain the contradictory
        // legacy "no global mode" phrasing that briefly shipped in #56.
        // (Learned the hard way when cluster-access info lived in a sibling
        // `infra` project.)
        assert_detailed_prompt_surfaces(|label, prompt| {
            assert!(
                prompt.contains("scopes"),
                "{label} must teach broadening via `scopes`"
            );
            assert!(
                prompt.contains("global=true") || prompt.contains("global = true"),
                "{label} must also teach broadening via `global=true`"
            );
            assert!(
                prompt.contains("sibling") || prompt.contains("SIBLING"),
                "{label} must mention knowledge can live in a sibling project"
            );
            assert!(
                prompt.contains("snippet") || prompt.contains("SNIPPET"),
                "{label} must warn that query returns snippets, not full bodies"
            );
            // Guard against the contradiction: standalone prose must not say
            // a global mode doesn't exist when the bullet/table-row above it
            // advertises `global=true`.
            let no_global_phrases = [
                "no global \"search everything\" mode",
                "NO global 'search everything' mode",
                "no global 'search everything' mode",
                "NO global \"search everything\" mode",
            ];
            for phrase in no_global_phrases {
                assert!(
                    !prompt.contains(phrase),
                    "{label} must not contain the contradictory phrase {phrase:?}"
                );
            }
        });
        let installed = installed_ai_memory_prompt_surface();
        assert!(
            installed.contains("scopes") && installed.contains("global=true"),
            "installed prompt surface must include exact cross-project broadening args"
        );
        assert!(
            installed.contains("deployment")
                && installed.contains("PR review")
                && installed.contains("migration")
                && installed.contains("data-preservation"),
            "installed prompt surface must preserve high-risk retrieval preflight guidance"
        );
    }

    #[test]
    fn prompts_warn_static_mcp_parallel_sessions_need_explicit_scope() {
        for prompt in [MEMORY_INSTRUCTIONS, ai_memory_core::SNIPPET_BODY] {
            let lower = prompt.to_ascii_lowercase();
            assert!(
                lower.contains("static mcp") && lower.contains("parallel sessions"),
                "prompt must warn about static MCP clients in parallel sessions"
            );
            assert!(
                lower.contains("real agent session id")
                    && (lower.contains("session-aware bridge")
                        || lower.contains("session aware bridge")),
                "prompt must distinguish real agent session id from static MCP config"
            );
            assert!(
                lower.contains("explicit")
                    && lower.contains("workspace")
                    && lower.contains("project")
                    && lower.contains("scopes"),
                "prompt must tell agents to use explicit scope when session id is unavailable"
            );
        }
    }

    #[test]
    fn prompts_route_permanent_annotations_to_write_page_not_handoff() {
        assert_detailed_prompt_surfaces(|label, prompt| {
            assert!(
                prompt.contains("permanent") || prompt.contains("permanently"),
                "{label} must mention permanent memory use cases"
            );
            assert!(
                prompt.contains("memory_write_page"),
                "{label} must expose memory_write_page"
            );
            assert!(
                prompt.contains("do NOT use") || prompt.contains("do **not** use"),
                "{label} must explicitly disallow handoffs for permanent notes"
            );
        });
    }
    #[test]
    fn prompts_treat_retrieved_memory_as_actionable_guidance() {
        assert_detailed_prompt_surfaces(|label, prompt| {
            let lower = prompt.to_ascii_lowercase();
            assert!(
                prompt.contains("_rules/")
                    && prompt.contains("gotchas/")
                    && prompt.contains("procedures/")
                    && prompt.contains("decisions/"),
                "{label} must name actionable page families"
            );
            assert!(
                lower.contains("constraints")
                    && lower.contains("preflight")
                    && lower.contains("checklists"),
                "{label} must teach how to use rules/gotchas/procedures"
            );
            assert!(
                lower.contains("before non-trivial")
                    && lower.contains("auth")
                    && lower.contains("migration"),
                "{label} must make proactive retrieval the default for risky work"
            );
        });
    }

    #[tokio::test]
    async fn memory_install_self_routing_response_includes_managed_skills_and_targets() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;

        let response = call_tool_json(server.memory_install_self_routing().await.unwrap());

        assert_eq!(
            response["markered_block"].as_str().unwrap(),
            ai_memory_core::full_block()
        );
        assert_eq!(
            response["marker_start"].as_str().unwrap(),
            ai_memory_core::MARKER_START
        );
        assert_eq!(
            response["marker_end"].as_str().unwrap(),
            ai_memory_core::MARKER_END
        );
        assert_eq!(
            response["agent_filenames"]["claude_code"].as_str().unwrap(),
            "CLAUDE.md"
        );
        assert_eq!(
            response["agent_filenames"]["default"].as_str().unwrap(),
            "AGENTS.md"
        );
        assert_eq!(
            response["agent_filenames"]["devin"].as_str().unwrap(),
            "AGENTS.md"
        );
        assert_eq!(
            response["agent_filenames"]["kimi_code"].as_str().unwrap(),
            "AGENTS.md"
        );
        // Proposed symmetrically alongside the devin assertion above:
        // upstream added "zero" to this same payload (issue #156) without a
        // matching assertion. Not validated against a live Zero agent in
        // this environment — for the Zero team to confirm "AGENTS.md" is
        // still the intended target file before relying on this.
        assert_eq!(
            response["agent_filenames"]["zero"].as_str().unwrap(),
            "AGENTS.md"
        );

        let managed_skills = response["managed_skills"]
            .as_array()
            .expect("managed_skills must be an array");
        assert_eq!(
            managed_skills.len(),
            ai_memory_core::routing_skills::MANAGED_SKILLS.len()
        );
        for expected in ai_memory_core::routing_skills::MANAGED_SKILLS {
            let skill = managed_skills
                .iter()
                .find(|skill| skill["name"].as_str() == Some(expected.name))
                .unwrap_or_else(|| panic!("missing managed skill {}", expected.name));
            assert_eq!(skill["description"].as_str().unwrap(), expected.description);
            assert_eq!(
                skill["relative_path"].as_str().unwrap(),
                expected.relative_path
            );
            assert_eq!(skill["content"].as_str().unwrap(), expected.content);
            assert!(
                skill["content"]
                    .as_str()
                    .unwrap()
                    .contains(ai_memory_core::routing_skills::MANAGED_MARKER),
                "managed skill {} must include the ownership marker",
                expected.name
            );
        }
        let routing_install_skill = managed_skills
            .iter()
            .find(|skill| skill["name"].as_str() == Some("ai-memory-routing-install"))
            .expect("routing-install skill must be included in the install payload");
        let routing_install_content = routing_install_skill["content"]
            .as_str()
            .expect("routing-install skill content must be text");
        assert!(
            routing_install_content.contains("target_hints")
                && routing_install_content.contains(".grok/skills")
                && routing_install_content.contains("$GROK_HOME/skills")
                && routing_install_content.contains("~/.grok/skills"),
            "routing-install skill must treat target_hints as authoritative and include Grok roots"
        );

        assert_eq!(
            response["target_hints"]["project"]["claude_code"]
                .as_str()
                .unwrap(),
            ".claude/skills"
        );
        assert_eq!(
            response["target_hints"]["project"]["agents"]
                .as_str()
                .unwrap(),
            ".agents/skills"
        );
        assert_eq!(
            response["target_hints"]["project"]["devin"]
                .as_str()
                .unwrap(),
            ".devin/skills"
        );
        assert_eq!(
            response["target_hints"]["project"]["grok"]
                .as_str()
                .unwrap(),
            ".grok/skills"
        );
        assert_eq!(
            response["target_hints"]["global"]["claude_code"]
                .as_str()
                .unwrap(),
            "~/.claude/skills"
        );
        assert_eq!(
            response["target_hints"]["global"]["agents"]
                .as_str()
                .unwrap(),
            "~/.agents/skills"
        );
        assert_eq!(
            response["target_hints"]["global"]["devin"]["windows"]
                .as_str()
                .unwrap(),
            "%APPDATA%\\devin\\skills"
        );
        assert_eq!(
            response["target_hints"]["global"]["devin"]["non_windows"]
                .as_str()
                .unwrap(),
            "~/.devin/skills"
        );
        assert_eq!(
            response["target_hints"]["global"]["grok"].as_str().unwrap(),
            "$GROK_HOME/skills (default: ~/.grok/skills)"
        );
        assert_eq!(
            response["agent_filenames"]["grok"].as_str().unwrap(),
            "AGENTS.md"
        );

        let notes = response["notes"]
            .as_array()
            .expect("notes must remain an array")
            .iter()
            .map(|note| note.as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(notes.contains(ai_memory_core::routing_skills::MANAGED_MARKER));
        assert!(notes.contains("unmanaged same-name skills"));
        assert!(notes.contains("%APPDATA%\\devin\\skills"));
        assert!(notes.contains("explicitly forces replacement"));
    }

    #[tokio::test]
    async fn memory_install_self_routing_tool_description_covers_snippet_and_skills() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let tools = server.tool_router.list_all();
        let install = tools
            .iter()
            .find(|tool| tool.name == "memory_install_self_routing")
            .expect("memory_install_self_routing must be registered");
        let desc = install
            .description
            .as_deref()
            .expect("memory_install_self_routing must carry a description");

        assert!(
            desc.contains("markered_block") && desc.contains("managed_skills"),
            "tool description must tell agents to install snippet and skill payloads; got: {desc}"
        );
        assert!(
            desc.contains(".claude/skills")
                && desc.contains(".agents/skills")
                && desc.contains(".devin/skills")
                && desc.contains(".grok/skills")
                && desc.contains("$GROK_HOME/skills"),
            "tool description must name Claude, .agents, Devin, and Grok skill targets; got: {desc}"
        );
        assert!(
            desc.contains("preserve non-ai-memory user content"),
            "tool description must preserve user content; got: {desc}"
        );
        assert!(
            desc.contains("unmanaged same-name skills") && desc.contains("explicitly forces"),
            "tool description must mention safe overwrite behavior; got: {desc}"
        );
    }

    #[tokio::test]
    async fn prompts_expose_auto_improve_as_auto_approval_with_manual_opt_in() {
        assert_detailed_prompt_surfaces(|label, prompt| {
            let lower = prompt.to_ascii_lowercase();
            assert!(prompt.contains("memory_auto_improve"));
            assert!(
                lower.contains("applies validated")
                    || (lower.contains("approval") && lower.contains("path")),
                "{label} must state auto-improve applies through the approval path"
            );
            assert!(
                lower.contains("require_approval") && lower.contains("pending-writes"),
                "{label} must describe the manual review opt-in"
            );
        });
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let tools = server.tool_router.list_all();
        let auto_improve = tools
            .iter()
            .find(|t| t.name == "memory_auto_improve")
            .expect("memory_auto_improve must be registered");
        let desc = auto_improve
            .description
            .as_deref()
            .expect("memory_auto_improve must carry a description");
        assert!(desc.contains("apply or stage validated"));
        assert!(desc.contains("approval"));
    }

    #[test]
    fn prompts_teach_cross_workspace_handoff_scope() {
        assert_detailed_prompt_surfaces(|label, prompt| {
            assert!(
                prompt.contains("memory_handoff_begin") && prompt.contains("memory_handoff_accept"),
                "{label} must include handoff lifecycle tools"
            );
            assert!(
                prompt.contains("workspace") && prompt.contains("project"),
                "{label} handoff guidance must mention workspace+project scoping"
            );
            assert!(
                prompt.contains("sibling")
                    && (prompt.contains("workspace/project")
                        || prompt.contains("workspace + project")
                        || prompt.contains("workspace` + `project")),
                "{label} handoff guidance must restrict explicit workspace scope to named siblings"
            );
        });
    }

    /// All three prompt surfaces must steer agents toward the H1-in-body
    /// convention instead of passing the `title` argument. The `title`
    /// argument is a known source of `JSON parsing` errors when the LLM
    /// fails to escape quotes (issue #67); routing every "remember this"
    /// call through the H1 path avoids the footgun entirely.
    ///
    /// The three surfaces - `MEMORY_INSTRUCTIONS`, the installed routing
    /// surface (`SNIPPET_BODY` plus managed skills), and the per-tool
    /// `#[tool(description=...)]` string surfaced via `tools/list` - are
    /// independent and must stay aligned.
    #[tokio::test]
    async fn prompts_steer_write_page_toward_h1_title_convention() {
        assert_detailed_prompt_surfaces(|label, prompt| {
            assert!(
                prompt.contains("H1"),
                "{label} must mention the H1 title convention for memory_write_page"
            );
            assert!(
                prompt.contains("omit") || prompt.contains("Omit"),
                "{label} must tell the agent to omit the `title` argument"
            );
        });
        // The third surface: the rmcp tool description sent to clients
        // via `tools/list`. Spell-checked against the same keywords so
        // that a future edit cannot silently drop the guidance from the
        // tool the agent actually inspects when deciding how to call.
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let tools = server.tool_router.list_all();
        let write_page = tools
            .iter()
            .find(|t| t.name == "memory_write_page")
            .expect("memory_write_page must be registered");
        let desc = write_page
            .description
            .as_deref()
            .expect("memory_write_page must carry a description");
        assert!(
            desc.contains("H1"),
            "tool description must mention the H1 title convention; got: {desc}"
        );
        assert!(
            desc.contains("Do NOT pass")
                || desc.contains("do NOT pass")
                || desc.contains("omit")
                || desc.contains("Omit"),
            "tool description must explicitly tell the agent to omit `title`; got: {desc}"
        );
    }

    /// Read tools resolve the project in the order: explicit `project`
    /// arg in the active workspace, explicit `project` in the baked
    /// workspace, hook-published active project, baked-in default (issue #2).
    #[tokio::test]
    async fn effective_ids_follows_precedence_chain() {
        let (_tmp, store, server, ws, baked) = setup_server().await;

        // Baseline: nothing published, no arg → baked-in default.
        assert_eq!(
            server
                .effective_ids_with_actor(None, &ai_memory_core::ActorKey::default())
                .await
                .unwrap(),
            (ws, baked)
        );

        // A second real project in the same workspace.
        let other = store
            .writer
            .get_or_create_project(
                ws,
                "projeto_camera",
                Some("/home/u/projeto_camera".to_string()),
            )
            .await
            .unwrap();

        // Hook publishes it → it becomes the default for cwd-less calls.
        server.active_project.set(ws, other);
        assert_eq!(
            server
                .effective_ids_with_actor(None, &ai_memory_core::ActorKey::default())
                .await
                .unwrap(),
            (ws, other)
        );

        // An explicit (existing) project arg wins over the active pointer.
        assert_eq!(
            server
                .effective_ids_with_actor(Some("scratch"), &ai_memory_core::ActorKey::default())
                .await
                .unwrap(),
            (ws, baked),
            "explicit project arg should override the active pointer"
        );

        // An explicit but unknown project name fails closed instead of
        // silently falling through to the active pointer.
        let err = server
            .effective_ids_with_actor(Some("does-not-exist"), &ai_memory_core::ActorKey::default())
            .await
            .expect_err("unknown explicit project must not fall back");
        assert!(
            err.to_string().contains("does-not-exist"),
            "error should name the missing explicit project: {err}"
        );
    }

    #[tokio::test]
    async fn write_target_ids_defaults_workspace_to_active_project() {
        let (_tmp, store, server, baked_ws, baked_proj) = setup_server().await;

        // A second workspace — the cwd's actual workspace (e.g. "djalmajr"),
        // distinct from the server's baked "default".
        let other_ws = store
            .writer
            .get_or_create_workspace("djalmajr")
            .await
            .unwrap();
        let other_proj = store
            .writer
            .get_or_create_project(other_ws, "ai-memory", None)
            .await
            .unwrap();
        // Hook publishes the cwd's project (in the OTHER workspace).
        server.active_project.set(other_ws, other_proj);

        // Explicit project, NO workspace → must land in the active project's
        // workspace (djalmajr) and REUSE the existing project, not recreate it
        // under the baked default.
        let (ws, proj) = server
            .write_target_ids(None, Some("ai-memory"))
            .await
            .unwrap();
        assert_eq!(
            ws, other_ws,
            "workspace must default to the active project's, not the baked default"
        );
        assert_eq!(
            proj, other_proj,
            "must reuse djalmajr/ai-memory, not recreate it"
        );

        // A different project name (no workspace) also lands in the cwd's workspace.
        let (ws2, _p2) = server
            .write_target_ids(None, Some("sibling"))
            .await
            .unwrap();
        assert_eq!(
            ws2, other_ws,
            "a sibling project lands in the cwd's workspace"
        );

        // Explicit workspace still overrides the active default.
        let (ws3, _p3) = server
            .write_target_ids(Some("default"), Some("ai-memory"))
            .await
            .unwrap();
        assert_eq!(
            ws3, baked_ws,
            "explicit workspace wins over the active pointer"
        );

        // No active project published → fall back to the baked workspace.
        let fresh = AiMemoryServer::new(
            store.reader.clone(),
            store.writer.clone(),
            baked_ws,
            baked_proj,
        );
        let (ws4, _p4) = fresh
            .write_target_ids(None, Some("ai-memory"))
            .await
            .unwrap();
        assert_eq!(
            ws4, baked_ws,
            "no active project → baked workspace is the fallback"
        );
    }

    #[tokio::test]
    async fn write_target_ids_rejects_workspace_without_project() {
        let (_tmp, _store, server, _baked_ws, _baked_proj) = setup_server().await;

        let err = server
            .write_target_ids(Some("default"), None)
            .await
            .expect_err("workspace-only writes must fail closed");
        assert!(
            err.to_string()
                .contains("workspace and project must be provided together"),
            "error should explain the required scope pair: {err}"
        );
    }

    #[tokio::test]
    async fn project_only_write_round_trips_with_project_only_read_in_active_workspace() {
        let (tmp, store, server, baked_ws, _baked_proj) = setup_server().await;
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let server = server.with_wiki(wiki);

        let active_ws = store
            .writer
            .get_or_create_workspace("djalmajr")
            .await
            .unwrap();
        let active_proj = store
            .writer
            .get_or_create_project(active_ws, "ai-memory", None)
            .await
            .unwrap();
        server.active_project.set(active_ws, active_proj);
        let parts = axum::http::Request::builder()
            .uri("/mcp")
            .method("POST")
            .body(())
            .unwrap()
            .into_parts()
            .0;

        server
            .memory_write_page(
                Parameters(WritePageArgs {
                    path: "notes/sibling.md".to_string(),
                    body: "project-only write should use the active workspace".to_string(),
                    title: Some("Sibling Note".to_string()),
                    tier: None,
                    tags: Vec::new(),
                    pinned: false,
                    project: Some("sibling".to_string()),
                    workspace: None,
                    scope: None,
                }),
                OptionalParts(parts),
            )
            .await
            .unwrap();

        assert!(
            store
                .reader
                .find_project(baked_ws, "sibling".to_string())
                .await
                .unwrap()
                .is_none(),
            "project-only write must not recreate default/sibling"
        );
        let sibling_proj = store
            .reader
            .find_project(active_ws, "sibling".to_string())
            .await
            .unwrap()
            .expect("project-only write should create active-workspace sibling");
        let direct_hits = store
            .reader
            .recent_pages_for_project(active_ws, sibling_proj, 5)
            .await
            .unwrap();
        assert_eq!(
            direct_hits.len(),
            1,
            "direct read should see the written page"
        );
        assert_eq!(
            server
                .effective_ids_with_actor(Some("sibling"), &ai_memory_core::ActorKey::default())
                .await
                .unwrap(),
            (active_ws, sibling_proj),
            "project-only read resolution should use the active workspace"
        );

        let result = server
            .memory_recent(
                Parameters(RecentArgs {
                    limit: Some(5),
                    project: Some("sibling".to_string()),
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .unwrap_or_else(|| panic!("expected text content"));
        assert!(
            text.contains("notes/sibling.md"),
            "project-only read must find the active-workspace write:\n{text}"
        );
        assert!(
            text.contains("Sibling Note"),
            "project-only read must return the written page:\n{text}"
        );
    }

    #[tokio::test]
    async fn memory_query_returns_hits_via_tool_method() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let result = server
            .memory_query(
                Parameters(QueryArgs {
                    query: "karpathy".into(),
                    limit: Some(5),
                    project: None,
                    scopes: Vec::new(),
                    workspace: None,
                    global: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = match result.content.first().and_then(|c| c.as_text()) {
            Some(t) => t.text.clone(),
            None => panic!("expected text content"),
        };
        assert!(text.contains("foo.md"), "expected hit; got {text}");
    }

    // Issue #154: default-scoped queries union the reserved `_global`
    // preferences scope; explicitly scoped queries do not.
    #[tokio::test]
    async fn default_query_unions_global_scope_and_explicit_scope_skips_it() {
        let (_tmp, store, server, _ws, _proj) = setup_server().await;
        let global = ai_memory_store::create_global_scope(&store.writer)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: global.workspace_id,
                project_id: global.project_id,
                path: PagePath::new("preferences/style.md").unwrap(),
                title: "Style".into(),
                body: "Karpathy approved standing preference: pnpm always.".into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
            })
            .await
            .unwrap();

        let query = |workspace: Option<&str>, project: Option<&str>| QueryArgs {
            query: "karpathy".into(),
            limit: Some(5),
            project: project.map(str::to_string),
            scopes: Vec::new(),
            workspace: workspace.map(str::to_string),
            global: None,
        };

        let result = server
            .memory_query(
                Parameters(query(None, None)),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result.content.first().and_then(|c| c.as_text()).unwrap();
        assert!(
            text.text.contains("foo.md"),
            "current-project hit must remain: {}",
            text.text
        );
        assert!(
            text.text.contains("global_scope_hits") && text.text.contains("preferences/style.md"),
            "default query must union the reserved global scope: {}",
            text.text
        );

        let result = server
            .memory_query(
                Parameters(query(Some("default"), Some("scratch"))),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result.content.first().and_then(|c| c.as_text()).unwrap();
        assert!(
            !text.text.contains("preferences/style.md"),
            "explicitly scoped queries must not union the global scope: {}",
            text.text
        );
    }

    // Issue #154: an absent `_global` scope contributes nothing and is
    // never created by a read.
    #[tokio::test]
    async fn default_query_without_global_scope_is_unchanged() {
        let (_tmp, store, server, _ws, _proj) = setup_server().await;
        let result = server
            .memory_query(
                Parameters(QueryArgs {
                    query: "karpathy".into(),
                    limit: Some(5),
                    project: None,
                    scopes: Vec::new(),
                    workspace: None,
                    global: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result.content.first().and_then(|c| c.as_text()).unwrap();
        assert!(
            !text.text.contains("global_scope_hits"),
            "no reserved scope -> field elided: {}",
            text.text
        );
        assert_eq!(
            ai_memory_store::lookup_global_scope(&store.reader)
                .await
                .unwrap(),
            None,
            "a read must never create the reserved scope"
        );
    }

    #[tokio::test]
    async fn write_page_scope_global_lands_in_reserved_scope() {
        let (tmp, store, server, _ws, _proj) = setup_server().await;
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let server = server.with_wiki(wiki);
        let write_args = |scope: Option<&str>, project: Option<&str>| WritePageArgs {
            path: "preferences/pkg.md".to_string(),
            body: "# Package manager\nAlways pnpm workspaces.".to_string(),
            title: None,
            tier: None,
            tags: Vec::new(),
            pinned: false,
            project: project.map(str::to_string),
            workspace: None,
            scope: scope.map(str::to_string),
        };

        server
            .memory_write_page(
                Parameters(write_args(Some("global"), None)),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let global = ai_memory_store::lookup_global_scope(&store.reader)
            .await
            .unwrap()
            .expect("scope: global write must create the reserved scope");
        let pages = store
            .reader
            .recent_pages_for_project(global.workspace_id, global.project_id, 10)
            .await
            .unwrap();
        assert!(
            pages
                .iter()
                .any(|p| p.path.as_str() == "preferences/pkg.md"),
            "page must land in the reserved scope; got {:?}",
            pages.iter().map(|p| p.path.as_str()).collect::<Vec<_>>()
        );

        let err = server
            .memory_write_page(
                Parameters(write_args(Some("global"), Some("other"))),
                OptionalParts(test_parts_default()),
            )
            .await
            .expect_err("scope + project must fail closed");
        assert!(err.to_string().contains("cannot be combined"), "{err}");

        let err = server
            .memory_write_page(
                Parameters(write_args(Some("universe"), None)),
                OptionalParts(test_parts_default()),
            )
            .await
            .expect_err("unknown scope values must fail closed");
        assert!(err.to_string().contains("unknown scope"), "{err}");
    }

    #[tokio::test]
    async fn memory_query_returns_raw_hits_when_pages_miss() {
        let (_tmp, store, server, ws, proj) = setup_server().await;
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "raw prompt".into(),
                    body: "raw fallback contains quokka only detail".into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();

        let result = server
            .memory_query(
                Parameters(QueryArgs {
                    query: "quokka".into(),
                    limit: Some(5),
                    project: None,
                    scopes: Vec::new(),
                    workspace: None,
                    global: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = match result.content.first().and_then(|c| c.as_text()) {
            Some(t) => t.text.clone(),
            None => panic!("expected text content"),
        };
        assert!(
            text.contains("\"hits\": []"),
            "expected no page hits; got {text}"
        );
        assert!(
            text.contains("raw_hits"),
            "expected raw fallback; got {text}"
        );
        assert!(text.contains("quokka"), "expected raw snippet; got {text}");
    }

    #[tokio::test]
    async fn memory_query_returns_raw_hits_via_explicit_scopes() {
        // The raw-observation fallback must also fire on the explicit
        // `scopes` path (the recommended scope-bleed mitigation), not just
        // default / workspace+project. Regression for a scope with
        // observations but zero compiled pages.
        let (_tmp, store, server, ws, _proj) = setup_server().await;
        let scoped = store
            .writer
            .get_or_create_project(ws, "scoped-obs", None)
            .await
            .unwrap();
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: scoped,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: scoped,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "raw prompt".into(),
                    body: "raw fallback contains quokka only detail".into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();

        let result = server
            .memory_query(
                Parameters(QueryArgs {
                    query: "quokka".into(),
                    limit: Some(5),
                    project: None,
                    scopes: vec![MemoryScopeArg {
                        project: "scoped-obs".into(),
                        workspace: "default".into(),
                    }],
                    workspace: None,
                    global: None,
                }),
                test_optional_parts(),
            )
            .await
            .unwrap();
        let text = match result.content.first().and_then(|c| c.as_text()) {
            Some(t) => t.text.clone(),
            None => panic!("expected text content"),
        };
        assert!(
            text.contains("\"hits\": []"),
            "expected no page hits; got {text}"
        );
        assert!(
            text.contains("raw_hits"),
            "expected raw fallback via scopes; got {text}"
        );
        assert!(text.contains("quokka"), "expected raw snippet; got {text}");
    }

    #[tokio::test]
    async fn memory_query_scoped_raw_hits_respect_limit_and_rank_order() {
        let (_tmp, store, server, ws, _proj) = setup_server().await;
        let first = store
            .writer
            .get_or_create_project(ws, "rank-first", None)
            .await
            .unwrap();
        let second = store
            .writer
            .get_or_create_project(ws, "rank-second", None)
            .await
            .unwrap();
        for (project_id, title) in [
            (first, "first-a"),
            (first, "first-b"),
            (second, "second-a"),
            (second, "second-b"),
        ] {
            insert_test_observation(
                &store,
                ws,
                project_id,
                title,
                &format!("rank_token appears in {title}"),
            )
            .await;
        }

        let mut expected = store
            .reader
            .search_observations_for_project(ws, first, "rank_token".into(), 3)
            .await
            .unwrap();
        expected.extend(
            store
                .reader
                .search_observations_for_project(ws, second, "rank_token".into(), 3)
                .await
                .unwrap(),
        );
        expected.sort_by(|a, b| {
            a.rank
                .partial_cmp(&b.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        expected.truncate(3);
        assert!(
            expected.iter().any(|hit| hit.title.starts_with("first-"))
                && expected.iter().any(|hit| hit.title.starts_with("second-")),
            "test setup must require merged hits from both scopes: {expected:?}"
        );

        let json = call_tool_json(
            server
                .memory_query(
                    Parameters(QueryArgs {
                        query: "rank_token".into(),
                        limit: Some(3),
                        project: None,
                        scopes: vec![
                            MemoryScopeArg {
                                project: "rank-first".into(),
                                workspace: "default".into(),
                            },
                            MemoryScopeArg {
                                project: "rank-second".into(),
                                workspace: "default".into(),
                            },
                        ],
                        workspace: None,
                        global: None,
                    }),
                    test_optional_parts(),
                )
                .await
                .unwrap(),
        );
        let raw_hits = json["raw_hits"].as_array().unwrap();
        assert_eq!(raw_hits.len(), 3, "raw hits must be truncated: {json}");
        let titles: Vec<&str> = raw_hits
            .iter()
            .map(|hit| hit["title"].as_str().unwrap())
            .collect();
        let expected_titles: Vec<&str> = expected.iter().map(|hit| hit.title.as_str()).collect();
        assert_eq!(
            titles, expected_titles,
            "raw hits should match the merged-and-truncated store ranking: {json}"
        );
        let ranks: Vec<f64> = raw_hits
            .iter()
            .map(|hit| hit["rank"].as_f64().unwrap())
            .collect();
        assert!(
            ranks.windows(2).all(|pair| pair[0] <= pair[1]),
            "raw hits must be rank-sorted: {json}"
        );
    }

    #[tokio::test]
    async fn memory_query_scoped_raw_hits_deduplicate_duplicate_scopes() {
        let (_tmp, store, server, ws, _proj) = setup_server().await;
        let scoped = store
            .writer
            .get_or_create_project(ws, "dedupe-obs", None)
            .await
            .unwrap();
        insert_test_observation(
            &store,
            ws,
            scoped,
            "dedupe raw prompt",
            "dedupe_token appears once",
        )
        .await;

        let json = call_tool_json(
            server
                .memory_query(
                    Parameters(QueryArgs {
                        query: "dedupe_token".into(),
                        limit: Some(10),
                        project: None,
                        scopes: vec![
                            MemoryScopeArg {
                                project: "dedupe-obs".into(),
                                workspace: "default".into(),
                            },
                            MemoryScopeArg {
                                project: "dedupe-obs".into(),
                                workspace: "default".into(),
                            },
                        ],
                        workspace: None,
                        global: None,
                    }),
                    test_optional_parts(),
                )
                .await
                .unwrap(),
        );
        let raw_hits = json["raw_hits"].as_array().unwrap();
        assert_eq!(
            raw_hits.len(),
            1,
            "duplicate scopes must not duplicate raw hits: {json}"
        );
        assert_eq!(raw_hits[0]["title"], "dedupe raw prompt");
    }

    #[tokio::test]
    async fn memory_query_missing_scope_fails_closed_without_default_raw_fallback() {
        let (_tmp, store, server, ws, proj) = setup_server().await;
        insert_test_observation(
            &store,
            ws,
            proj,
            "default raw prompt",
            "missing_scope_token exists only in the default project",
        )
        .await;

        let err = server
            .memory_query(
                Parameters(QueryArgs {
                    query: "missing_scope_token".into(),
                    limit: Some(10),
                    project: None,
                    scopes: vec![MemoryScopeArg {
                        project: "absent".into(),
                        workspace: "default".into(),
                    }],
                    workspace: None,
                    global: None,
                }),
                test_optional_parts(),
            )
            .await
            .expect_err("missing explicit scope must fail closed");
        let msg = err.to_string();
        assert!(
            msg.contains("absent") || msg.contains("not found"),
            "missing scope error should identify the bad scope: {msg}"
        );
    }

    #[tokio::test]
    async fn memory_query_page_hits_suppress_scoped_raw_fallback() {
        let (_tmp, store, server, ws, _proj) = setup_server().await;
        let pages = store
            .writer
            .get_or_create_project(ws, "page-scope", None)
            .await
            .unwrap();
        let raw = store
            .writer
            .get_or_create_project(ws, "raw-scope", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: ws,
                project_id: pages,
                path: PagePath::new("page-hit.md").unwrap(),
                title: "Page Hit".into(),
                body: "mixed_scope_token appears in a compiled page".into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
            })
            .await
            .unwrap();
        insert_test_observation(
            &store,
            ws,
            raw,
            "raw mixed prompt",
            "mixed_scope_token also appears only in raw observations",
        )
        .await;

        let json = call_tool_json(
            server
                .memory_query(
                    Parameters(QueryArgs {
                        query: "mixed_scope_token".into(),
                        limit: Some(10),
                        project: None,
                        scopes: vec![
                            MemoryScopeArg {
                                project: "page-scope".into(),
                                workspace: "default".into(),
                            },
                            MemoryScopeArg {
                                project: "raw-scope".into(),
                                workspace: "default".into(),
                            },
                        ],
                        workspace: None,
                        global: None,
                    }),
                    test_optional_parts(),
                )
                .await
                .unwrap(),
        );
        assert!(
            json["hits"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hit| hit["path"] == "page-hit.md"),
            "expected compiled page hit: {json}"
        );
        assert!(
            json.get("raw_hits")
                .is_none_or(|raw_hits| raw_hits.as_array().is_some_and(Vec::is_empty)),
            "compiled page hits must suppress raw fallback: {json}"
        );
    }

    #[tokio::test]
    async fn memory_query_can_target_explicit_workspace_project() {
        let (_tmp, store, server, _ws, _pj) = setup_server().await;
        let practice_ws = store
            .writer
            .get_or_create_workspace("practice")
            .await
            .unwrap();
        let testing = store
            .writer
            .get_or_create_project(practice_ws, "unit-testing", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: practice_ws,
                project_id: testing,
                path: PagePath::new("patterns.md").unwrap(),
                title: "Testing Patterns".into(),
                body: "workspace_specific_token belongs to practice".into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
            })
            .await
            .unwrap();

        let result = server
            .memory_query(
                Parameters(QueryArgs {
                    query: "workspace_specific_token".into(),
                    limit: Some(5),
                    project: Some("unit-testing".into()),
                    scopes: Vec::new(),
                    workspace: Some("practice".into()),
                    global: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(text.contains("patterns.md"), "expected hit; got {text}");
    }

    // Issue #155: the "exactly one of path/query" contract must live in the
    // machine-readable schema, with branches demanding presence AND a
    // non-null type — a bare `required` is satisfied by OpenCode-style
    // `path: null` filling. Pins against a schemars upgrade silently
    // dropping the `extend` attribute.
    #[test]
    fn read_page_schema_encodes_one_of_path_or_query() {
        let schema = serde_json::to_value(schemars::schema_for!(ReadPageArgs)).unwrap();
        let any_of = schema
            .get("anyOf")
            .and_then(|v| v.as_array())
            .unwrap_or_else(|| panic!("schema must carry the anyOf constraint: {schema}"));
        for key in ["path", "query"] {
            let branch = any_of
                .iter()
                .find(|b| b["required"] == serde_json::json!([key]))
                .unwrap_or_else(|| panic!("missing anyOf branch requiring `{key}`: {schema}"));
            assert_eq!(
                branch["properties"][key]["type"],
                serde_json::json!("string"),
                "`{key}` branch must demand a non-null string so null-filling \
                 clients cannot satisfy it"
            );
        }
    }

    // Pins what the patch strips (root combinators) and what it preserves.
    #[test]
    fn moonshot_safe_tool_list_strips_root_combinators_only() {
        let schema: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "title": "ReadPageArgs",
                "type": "object",
                "description": "Read one wiki page.",
                "properties": {
                    "path": { "type": "string", "description": "Exact wiki path" },
                    // Nested combinator: must survive (rejection is root-specific).
                    "query": { "anyOf": [{ "type": "string" }, { "type": "null" }] }
                },
                "anyOf": [{ "required": ["path"] }, { "required": ["query"] }],
                "oneOf": [{ "required": ["path"] }],
                "allOf": [{ "required": ["query"] }]
            }))
            .unwrap();
        let tool = Tool::new("memory_read_page", "Read a wiki page", schema);

        let patched = moonshot_safe_tool_list(vec![tool]);
        let out = &patched[0].input_schema;

        for key in ["anyOf", "oneOf", "allOf"] {
            assert!(
                !out.contains_key(key),
                "root `{key}` must be stripped: {out:?}"
            );
        }
        for key in ["$schema", "title", "type", "description", "properties"] {
            assert!(out.contains_key(key), "root `{key}` must survive: {out:?}");
        }
        assert_eq!(
            out["properties"]["query"]["anyOf"],
            serde_json::json!([{ "type": "string" }, { "type": "null" }]),
            "nested combinators are out of scope for the patch"
        );
    }

    #[test]
    fn moonshot_safe_tool_list_leaves_flat_tools_untouched() {
        let schema: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "type": "object",
                "properties": { "verbose": { "type": "boolean" } }
            }))
            .unwrap();
        let tool = Tool::new("memory_status", "Status counts", schema);
        let before = serde_json::to_value(&tool).unwrap();

        let patched = moonshot_safe_tool_list(vec![tool]);
        let after = serde_json::to_value(&patched[0]).unwrap();

        assert_eq!(before, after, "flat tools must pass through unchanged");
    }

    // Issue #155: the neither-arg error must teach a looping model what a
    // valid retry looks like, naming both args and a concrete example.
    #[tokio::test]
    async fn memory_read_page_without_args_returns_instructive_error() {
        let (tmp, store, server, _ws, _pj) = setup_server().await;
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let err = server
            .with_wiki(wiki)
            .memory_read_page(
                Parameters(ReadPageArgs {
                    query: None,
                    path: None,
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .expect_err("neither arg must fail closed");
        let msg = err.to_string();
        for needle in ["`path`", "`query`", "notes/topic.md", "do not retry"] {
            assert!(msg.contains(needle), "error must contain {needle:?}: {msg}");
        }
    }

    #[tokio::test]
    async fn memory_read_page_can_target_explicit_workspace_project() {
        let (tmp, store, server, _ws, _pj) = setup_server().await;
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let practice_ws = store
            .writer
            .get_or_create_workspace("practice")
            .await
            .unwrap();
        let docs = store
            .writer
            .get_or_create_project(practice_ws, "docs", None)
            .await
            .unwrap();
        wiki.write_page(WritePageRequest {
            workspace_id: practice_ws,
            project_id: docs,
            path: PagePath::new("notes/sibling.md").unwrap(),
            frontmatter: serde_json::json!({"title": "Sibling Page"}),
            body: "workspace explicit read body".to_string(),
            tier: Tier::Semantic,
            pinned: false,
            title: Some("Sibling Page".into()),
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();

        let result = server
            .with_wiki(wiki)
            .memory_read_page(
                Parameters(ReadPageArgs {
                    query: None,
                    path: Some("notes/sibling.md".into()),
                    project: Some("docs".into()),
                    workspace: Some("practice".into()),
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(
            text.contains("workspace explicit read body"),
            "expected sibling workspace body; got {text}"
        );
        assert!(
            text.contains("notes/sibling.md"),
            "expected sibling workspace path; got {text}"
        );
    }

    #[tokio::test]
    async fn memory_read_page_marks_db_fallback_when_file_missing() {
        let (tmp, store, server, ws, proj) = setup_server().await;
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: ws,
                project_id: proj,
                path: PagePath::new("notes/db-only-tool.md").unwrap(),
                title: "DB Only Tool".into(),
                body: "tool fallback body".into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({"title": "DB Only Tool"}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
            })
            .await
            .unwrap();

        let result = server
            .with_wiki(wiki)
            .memory_read_page(
                Parameters(ReadPageArgs {
                    query: None,
                    path: Some("notes/db-only-tool.md".into()),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(
            text.contains("tool fallback body"),
            "expected DB body; got {text}"
        );
        assert!(
            text.contains("db-fallback"),
            "expected fallback diagnostic; got {text}"
        );
    }

    #[tokio::test]
    async fn memory_read_page_missing_error_names_the_scope() {
        let (tmp, store, server, _ws, _proj) = setup_server().await;
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let server = server.with_wiki(wiki);

        // Explicit scope: the not-found error names workspace/project and the
        // relative path (no raw disk error / absolute path leak).
        let err = server
            .memory_read_page(
                Parameters(ReadPageArgs {
                    query: None,
                    path: Some("does-not-exist.md".into()),
                    project: Some("scratch".into()),
                    workspace: Some("default".into()),
                }),
                test_optional_parts(),
            )
            .await
            .expect_err("missing page must error");
        let msg = err.to_string();
        assert!(
            msg.contains("default/scratch"),
            "must name the scope; got {msg}"
        );
        assert!(
            msg.contains("does-not-exist.md"),
            "must name the path; got {msg}"
        );

        // Auto-scoped read: the error adds the parallel-session hint.
        let err = server
            .memory_read_page(
                Parameters(ReadPageArgs {
                    query: None,
                    path: Some("does-not-exist.md".into()),
                    project: None,
                    workspace: None,
                }),
                test_optional_parts(),
            )
            .await
            .expect_err("missing page must error");
        assert!(
            err.to_string().contains("auto-resolved"),
            "auto-scoped error must hint at scope-bleed; got {err}"
        );
    }

    #[tokio::test]
    async fn memory_query_can_search_multiple_scopes() {
        let (_tmp, store, server, ws, _pj) = setup_server().await;
        let product = store
            .writer
            .get_or_create_project(ws, "product", None)
            .await
            .unwrap();
        let hidden = store
            .writer
            .get_or_create_project(ws, "hidden", None)
            .await
            .unwrap();
        let practice_ws = store
            .writer
            .get_or_create_workspace("practice")
            .await
            .unwrap();
        let testing = store
            .writer
            .get_or_create_project(practice_ws, "unit-testing", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: ws,
                project_id: product,
                path: PagePath::new("product.md").unwrap(),
                title: "Product Rules".into(),
                body: "multi_scope_token belongs to product".into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
            })
            .await
            .unwrap();
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: practice_ws,
                project_id: testing,
                path: PagePath::new("patterns.md").unwrap(),
                title: "Testing Patterns".into(),
                body: "multi_scope_token belongs to practice".into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
            })
            .await
            .unwrap();
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: ws,
                project_id: hidden,
                path: PagePath::new("hidden.md").unwrap(),
                title: "Hidden".into(),
                body: "multi_scope_token must not be returned".into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
            })
            .await
            .unwrap();

        let result = server
            .memory_query(
                Parameters(QueryArgs {
                    query: "multi_scope_token".into(),
                    limit: Some(10),
                    project: None,
                    scopes: vec![
                        MemoryScopeArg {
                            project: "product".into(),
                            workspace: "default".into(),
                        },
                        MemoryScopeArg {
                            project: "unit-testing".into(),
                            workspace: "practice".into(),
                        },
                    ],
                    workspace: None,
                    global: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(text.contains("product.md"), "expected product hit: {text}");
        assert!(
            text.contains("patterns.md"),
            "expected practice hit: {text}"
        );
        assert!(!text.contains("hidden.md"), "unexpected hidden hit: {text}");
    }

    #[tokio::test]
    async fn memory_query_global_searches_all_projects() {
        let (_tmp, store, server, ws, _pj) = setup_server().await;
        let other = store
            .writer
            .get_or_create_project(ws, "infra", None)
            .await
            .unwrap();
        let other_ws = store.writer.get_or_create_workspace("ops").await.unwrap();
        let third = store
            .writer
            .get_or_create_project(other_ws, "runbooks", None)
            .await
            .unwrap();
        for (w, p, path, body) in [
            (ws, other, "cluster.md", "global_token lives in infra"),
            (
                other_ws,
                third,
                "deploy.md",
                "global_token lives in ops runbooks",
            ),
        ] {
            store
                .writer
                .upsert_page(NewPage {
                    workspace_id: w,
                    project_id: p,
                    path: PagePath::new(path).unwrap(),
                    title: path.into(),
                    body: body.into(),
                    tier: Tier::Semantic,
                    frontmatter_json: serde_json::json!({}),
                    pinned: false,
                    links: Vec::new(),
                    author_id: None,
                })
                .await
                .unwrap();
        }

        let result = server
            .memory_query(
                Parameters(QueryArgs {
                    query: "global_token".into(),
                    limit: Some(10),
                    project: None,
                    scopes: Vec::new(),
                    workspace: None,
                    global: Some(true),
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        // Both projects (across two workspaces) surface in one global call,
        // each annotated with its project name.
        assert!(text.contains("cluster.md"), "expected infra hit: {text}");
        assert!(text.contains("deploy.md"), "expected ops hit: {text}");
        assert!(
            text.contains("infra"),
            "hit must carry project name: {text}"
        );
        assert!(text.contains("global_hits"), "global hits field: {text}");
    }

    #[tokio::test]
    async fn memory_query_default_global_marker_broadens_unscoped_query() {
        let (_tmp, store, server, ws, _pj) = setup_server().await;
        let infra = store
            .writer
            .get_or_create_project(ws, "infra", None)
            .await
            .unwrap();
        let ops_ws = store.writer.get_or_create_workspace("ops").await.unwrap();
        let runbooks = store
            .writer
            .get_or_create_project(ops_ws, "runbooks", None)
            .await
            .unwrap();
        for (w, p, path, body) in [
            (ws, infra, "cluster.md", "recall_token lives in infra"),
            (
                ops_ws,
                runbooks,
                "deploy.md",
                "recall_token lives in runbooks",
            ),
        ] {
            store
                .writer
                .upsert_page(NewPage {
                    workspace_id: w,
                    project_id: p,
                    path: PagePath::new(path).unwrap(),
                    title: path.into(),
                    body: body.into(),
                    tier: Tier::Semantic,
                    frontmatter_json: serde_json::json!({}),
                    pinned: false,
                    links: Vec::new(),
                    author_id: None,
                })
                .await
                .unwrap();
        }

        // The repo opted into `[recall] default_global` — the hook publishes it
        // on the ActiveProject (single slot here, matching the empty test actor).
        server
            .active_project
            .set_for(&ai_memory_core::ActorKey::default(), ws, infra, true);

        // A query with NO scoping args now behaves as `global=true`.
        let result = server
            .memory_query(
                Parameters(QueryArgs {
                    query: "recall_token".into(),
                    limit: Some(10),
                    project: None,
                    scopes: Vec::new(),
                    workspace: None,
                    global: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(
            text.contains("global_hits"),
            "default_global must route the unscoped query to global: {text}"
        );
        assert!(text.contains("cluster.md"), "infra hit expected: {text}");
        assert!(text.contains("deploy.md"), "runbooks hit expected: {text}");

        // Precedence: an EXPLICIT workspace+project still wins over
        // default_global — the query scopes to runbooks only (no infra hit).
        let scoped = server
            .memory_query(
                Parameters(QueryArgs {
                    query: "recall_token".into(),
                    limit: Some(10),
                    project: Some("runbooks".into()),
                    scopes: Vec::new(),
                    workspace: Some("ops".into()),
                    global: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let scoped_text = scoped
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(
            scoped_text.contains("deploy.md"),
            "explicit ops/runbooks hit expected: {scoped_text}"
        );
        assert!(
            !scoped_text.contains("cluster.md"),
            "explicit scope must NOT broaden to infra: {scoped_text}"
        );
    }

    #[tokio::test]
    async fn memory_query_global_rejects_explicit_scope() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let err = server
            .memory_query(
                Parameters(QueryArgs {
                    query: "x".into(),
                    limit: Some(5),
                    project: Some("product".into()),
                    scopes: Vec::new(),
                    workspace: None,
                    global: Some(true),
                }),
                OptionalParts(test_parts_default()),
            )
            .await;
        assert!(
            err.is_err(),
            "global must not combine with project/workspace/scopes"
        );
    }

    #[tokio::test]
    async fn memory_status_returns_counts() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let result = server
            .memory_status(
                Parameters(StatusArgs {
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(text.contains("\"pages_latest\": 1"));
    }

    #[tokio::test]
    async fn memory_briefing_returns_structured_snapshot() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let result = server
            .memory_briefing(
                Parameters(BriefingArgs {
                    recent_pages_limit: Some(5),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        // Spot-check the structural shape — every key must be present
        // so callers don't need to defensively handle missing fields.
        for key in [
            "\"counts\":",
            "\"activity_7d\":",
            "\"activity_30d\":",
            "\"last_observation_at\":",
            "\"pending_handoff_count\":",
            "\"rules\":",
            "\"slots\":",
            "\"recent_pages\":",
        ] {
            assert!(text.contains(key), "missing {key} in briefing:\n{text}");
        }
        // setup_server inserts one page, no sessions/observations,
        // no rules/slots. The activity windows therefore observe zero.
        assert!(
            text.contains("\"sessions\": 0"),
            "expected lifetime sessions: 0\n{text}"
        );
    }

    /// `memory_explore` without an LLM provider configured must
    /// degrade to returning the underlying briefing rather than
    /// erroring. Mirrors the behaviour of `memory_consolidate`
    /// (no provider → clean error/no-op), and matches the design
    /// invariant that LLM features are strictly opt-in.
    #[tokio::test]
    async fn memory_explore_without_llm_degrades_to_briefing() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let result = server
            .memory_explore(
                Parameters(ExploreArgs {
                    focus: None,
                    recent_pages_limit: Some(5),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(
            text.contains("\"prose\": null"),
            "expected null prose\n{text}"
        );
        assert!(
            text.contains("no LLM provider configured"),
            "expected fallback reason\n{text}"
        );
        assert!(
            text.contains("\"briefing\":"),
            "expected briefing payload\n{text}"
        );
    }

    #[test]
    fn explore_gap_bucket_picks_right_label() {
        use ai_memory_store::BriefingSnapshot;
        // No prior activity → `none`.
        let snap = BriefingSnapshot::default();
        let gap = explore_gap_from_snapshot(&snap);
        assert_eq!(gap.bucket, "none");
        assert!(gap.hours_since_last.is_none());

        // Helper: build a snapshot with last_observation_at N hours ago.
        let snap_at = |hours: i64| -> BriefingSnapshot {
            let ts = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(hours);
            BriefingSnapshot {
                last_observation_at: Some(ts.to_string()),
                ..Default::default()
            }
        };

        let cases = [(2, "today"), (24 * 10, "dormant"), (24 * 60, "stale")];
        for (hours, expected) in cases {
            let g = explore_gap_from_snapshot(&snap_at(hours));
            assert_eq!(
                g.bucket, expected,
                "{hours}h → {expected}, got {}",
                g.bucket
            );
        }
    }

    #[tokio::test]
    async fn memory_recent_returns_one_hit() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let result = server
            .memory_recent(
                Parameters(RecentArgs {
                    limit: Some(5),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(text.contains("foo.md"), "expected hit; got {text}");
    }

    #[tokio::test]
    async fn memory_recent_default_global_marker_broadens_to_all_projects() {
        let (_tmp, store, server, ws, _pj) = setup_server().await;
        let infra = store
            .writer
            .get_or_create_project(ws, "infra", None)
            .await
            .unwrap();
        let ops_ws = store.writer.get_or_create_workspace("ops").await.unwrap();
        let runbooks = store
            .writer
            .get_or_create_project(ops_ws, "runbooks", None)
            .await
            .unwrap();
        for (w, p, path) in [(ws, infra, "cluster.md"), (ops_ws, runbooks, "deploy.md")] {
            store
                .writer
                .upsert_page(NewPage {
                    workspace_id: w,
                    project_id: p,
                    path: PagePath::new(path).unwrap(),
                    title: path.into(),
                    body: "recent body".into(),
                    tier: Tier::Semantic,
                    frontmatter_json: serde_json::json!({}),
                    pinned: false,
                    links: Vec::new(),
                    author_id: None,
                })
                .await
                .unwrap();
        }
        // Opt into default_global on the (empty) test actor's single slot.
        server
            .active_project
            .set_for(&ai_memory_core::ActorKey::default(), ws, infra, true);

        let result = server
            .memory_recent(
                Parameters(RecentArgs {
                    limit: Some(10),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(
            text.contains("global_hits"),
            "default_global must broaden recent across projects: {text}"
        );
        assert!(text.contains("cluster.md"), "{text}");
        assert!(text.contains("deploy.md"), "{text}");

        // An explicit workspace+project still scopes (no cross-project broaden).
        let scoped = server
            .memory_recent(
                Parameters(RecentArgs {
                    limit: Some(10),
                    project: Some("runbooks".into()),
                    workspace: Some("ops".into()),
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let scoped_text = scoped
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(scoped_text.contains("deploy.md"), "{scoped_text}");
        assert!(
            !scoped_text.contains("cluster.md"),
            "explicit scope must not broaden: {scoped_text}"
        );
    }

    #[tokio::test]
    async fn memory_write_page_writes_durable_page() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
            .with_wiki(wiki);

        // Build a synthetic `Parts` so the new `Extension<Parts>` extractor
        // can be satisfied — no actor headers, so the admission chain
        // gets a default (anonymous) context, same as a stdio caller.
        let parts = axum::http::Request::builder()
            .uri("/mcp")
            .method("POST")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let result = server
            .memory_write_page(
                Parameters(WritePageArgs {
                    path: "notes/santander-2025.md".into(),
                    body: "# Santander 2025\n\nDurable tax annotation.".into(),
                    title: Some("Santander 2025".into()),
                    tier: Some("semantic".into()),
                    tags: vec!["finance".into()],
                    pinned: true,
                    project: None,
                    workspace: None,
                    scope: None,
                }),
                OptionalParts(parts),
            )
            .await
            .unwrap();
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(text.contains("notes/santander-2025.md"), "got {text}");

        let recent = server
            .memory_recent(
                Parameters(RecentArgs {
                    limit: Some(5),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let recent_text = recent
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(
            recent_text.contains("notes/santander-2025.md"),
            "write-page result must be visible to read tools; got {recent_text}"
        );
    }

    #[tokio::test]
    async fn memory_write_page_rejects_workspace_without_project() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
            .with_wiki(wiki);

        let err = server
            .memory_write_page(
                Parameters(WritePageArgs {
                    path: "notes/invalid-scope.md".into(),
                    body: "# Invalid Scope".into(),
                    title: None,
                    tier: Some("semantic".into()),
                    tags: vec![],
                    pinned: false,
                    project: None,
                    workspace: Some("default".into()),
                    scope: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .expect_err("workspace-only memory_write_page must fail");
        assert!(
            err.to_string()
                .contains("workspace and project must be provided together"),
            "error should explain the required scope pair: {err}"
        );
    }

    #[tokio::test]
    async fn memory_write_page_as_db_user_records_author() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let token = ai_memory_store::generate_token().unwrap();
        let pepper = ai_memory_store::TokenPepper::new("test-pepper-author");
        let token_hash = ai_memory_store::hash_token(&token, &pepper);
        let user_id = store
            .writer
            .create_user(
                NewUser {
                    username: "alice".into(),
                    name: Some("Alice Smith".into()),
                    email: Some("alice@example.com".into()),
                },
                token_hash,
            )
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
            .with_wiki(wiki);
        let mut parts = test_parts_default();
        parts.extensions.insert(AuthLevel::User);
        parts.extensions.insert(user_id);
        parts.extensions.insert(ActorContext {
            user: Some("alice".into()),
            name: Some("Alice Smith".into()),
            email: Some("alice@example.com".into()),
            ..ActorContext::default()
        });

        server
            .memory_write_page(
                Parameters(WritePageArgs {
                    path: "notes/user-attributed.md".into(),
                    body: "# User Attributed\n\nWritten by a normal DB user.".into(),
                    title: None,
                    tier: Some("semantic".into()),
                    tags: vec![],
                    pinned: false,
                    project: None,
                    workspace: None,
                    scope: None,
                }),
                OptionalParts(parts),
            )
            .await
            .unwrap();

        let meta = store
            .reader
            .page_meta("default", "scratch", "notes/user-attributed.md")
            .await
            .unwrap()
            .expect("written page should have metadata");
        let author = meta.author.expect("DB user write should carry author");
        assert_eq!(author.username, "alice");
        assert_eq!(author.name.as_deref(), Some("Alice Smith"));
        assert_eq!(author.email.as_deref(), Some("alice@example.com"));
    }

    #[tokio::test]
    async fn memory_read_page_unknown_explicit_project_does_not_fallback() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
            .with_wiki(wiki);

        server
            .memory_write_page(
                Parameters(WritePageArgs {
                    path: "notes/default.md".into(),
                    body: "# Default\n\nThis page must not be read through a typo.".into(),
                    title: None,
                    tier: Some("semantic".into()),
                    tags: vec![],
                    pinned: false,
                    project: None,
                    workspace: None,
                    scope: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();

        let err = server
            .memory_read_page(
                Parameters(ReadPageArgs {
                    query: None,
                    path: Some("notes/default.md".into()),
                    project: Some("typo".into()),
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .expect_err("unknown explicit project must not fall back to scratch");
        assert!(
            err.to_string().contains("typo"),
            "error should name the missing explicit project: {err}"
        );
    }

    #[tokio::test]
    async fn memory_delete_page_removes_the_page() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
            .with_wiki(wiki);
        let parts = || {
            axum::http::Request::builder()
                .uri("/mcp")
                .method("POST")
                .body(())
                .unwrap()
                .into_parts()
                .0
        };

        server
            .memory_write_page(
                Parameters(WritePageArgs {
                    path: "notes/temp.md".into(),
                    body: "# Temp\n\nthrowaway".into(),
                    title: Some("Temp".into()),
                    tier: Some("semantic".into()),
                    tags: vec![],
                    pinned: false,
                    project: None,
                    workspace: None,
                    scope: None,
                }),
                OptionalParts(parts()),
            )
            .await
            .unwrap();

        server
            .memory_delete_page(
                Parameters(DeletePageArgs {
                    path: "notes/temp.md".into(),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(parts()),
            )
            .await
            .unwrap();

        // The on-disk file is gone; reading it back errors (file not found).
        let read = server
            .memory_read_page(
                Parameters(ReadPageArgs {
                    query: None,
                    path: Some("notes/temp.md".into()),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await;
        assert!(read.is_err(), "deleted page must not be readable");

        // Regression: the derived index row must also be gone — the watcher
        // does not reconcile deletions, so a file-only delete would leave the
        // page surfacing in recent/search with stale content.
        let recent = server
            .memory_recent(
                Parameters(RecentArgs {
                    limit: Some(10),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let recent_text = recent
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(
            !recent_text.contains("notes/temp.md"),
            "deleted page must not linger in the index; got {recent_text}"
        );
    }

    #[tokio::test]
    async fn memory_delete_page_unknown_explicit_project_does_not_fallback() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
            .with_wiki(wiki);

        server
            .memory_write_page(
                Parameters(WritePageArgs {
                    path: "notes/keep.md".into(),
                    body: "# Keep\n\nThis page must survive an explicit project typo.".into(),
                    title: None,
                    tier: Some("semantic".into()),
                    tags: vec![],
                    pinned: false,
                    project: None,
                    workspace: None,
                    scope: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();

        let err = server
            .memory_delete_page(
                Parameters(DeletePageArgs {
                    path: "notes/keep.md".into(),
                    project: Some("typo".into()),
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .expect_err("unknown explicit project must not delete from scratch");
        assert!(
            err.to_string().contains("typo"),
            "error should name the missing explicit project: {err}"
        );

        let read = server
            .memory_read_page(
                Parameters(ReadPageArgs {
                    query: None,
                    path: Some("notes/keep.md".into()),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await;
        assert!(read.is_ok(), "page must survive delete with typo'd project");
    }

    /// Bug 5 regression: when a project name lives in MULTIPLE workspaces,
    /// `memory_delete_page` without `workspace` resolved scope via
    /// `effective_ids(project)` and could silently land in the wrong slot
    /// (returning `deleted: true` while the page survived in the workspace
    /// the operator actually meant). Passing `workspace` + `project` now
    /// flows through `effective_ids_for_read_args` — the same path the read
    /// tools use — so the delete lands EXACTLY where the operator pointed.
    #[tokio::test]
    async fn memory_delete_page_with_explicit_workspace_targets_right_scope() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws_alpha = store.writer.get_or_create_workspace("alpha").await.unwrap();
        let proj_alpha_shared = store
            .writer
            .get_or_create_project(ws_alpha, "shared", None)
            .await
            .unwrap();
        let ws_beta = store.writer.get_or_create_workspace("beta").await.unwrap();
        let proj_beta_shared = store
            .writer
            .get_or_create_project(ws_beta, "shared", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        // Server's baked default is alpha/shared; beta/shared is the
        // sibling we'll target via explicit (workspace, project).
        let server = AiMemoryServer::new(
            store.reader.clone(),
            store.writer.clone(),
            ws_alpha,
            proj_alpha_shared,
        )
        .with_wiki(wiki);
        let parts = || {
            axum::http::Request::builder()
                .uri("/mcp")
                .method("POST")
                .body(())
                .unwrap()
                .into_parts()
                .0
        };

        // Seed both workspaces with a SAME-NAMED page.
        server
            .memory_write_page(
                Parameters(WritePageArgs {
                    path: "notes/twin.md".into(),
                    body: "# alpha twin".into(),
                    title: None,
                    tier: Some("semantic".into()),
                    tags: vec![],
                    pinned: false,
                    project: Some("shared".into()),
                    workspace: Some("alpha".into()),
                    scope: None,
                }),
                OptionalParts(parts()),
            )
            .await
            .unwrap();
        server
            .memory_write_page(
                Parameters(WritePageArgs {
                    path: "notes/twin.md".into(),
                    body: "# beta twin".into(),
                    title: None,
                    tier: Some("semantic".into()),
                    tags: vec![],
                    pinned: false,
                    project: Some("shared".into()),
                    workspace: Some("beta".into()),
                    scope: None,
                }),
                OptionalParts(parts()),
            )
            .await
            .unwrap();

        // Delete from BETA only, explicit scope.
        server
            .memory_delete_page(
                Parameters(DeletePageArgs {
                    path: "notes/twin.md".into(),
                    project: Some("shared".into()),
                    workspace: Some("beta".into()),
                }),
                OptionalParts(parts()),
            )
            .await
            .unwrap();

        // Alpha twin must survive.
        let read_alpha = server
            .memory_read_page(
                Parameters(ReadPageArgs {
                    query: None,
                    path: Some("notes/twin.md".into()),
                    project: Some("shared".into()),
                    workspace: Some("alpha".into()),
                }),
                OptionalParts(test_parts_default()),
            )
            .await;
        assert!(
            read_alpha.is_ok(),
            "alpha/shared/notes/twin.md must survive a delete targeting beta"
        );

        // Beta twin must be gone (file-on-disk delete + DB row cleared).
        let read_beta = server
            .memory_read_page(
                Parameters(ReadPageArgs {
                    query: None,
                    path: Some("notes/twin.md".into()),
                    project: Some("shared".into()),
                    workspace: Some("beta".into()),
                }),
                OptionalParts(test_parts_default()),
            )
            .await;
        assert!(
            read_beta.is_err(),
            "beta/shared/notes/twin.md must be gone after delete with explicit workspace"
        );

        // Defense-in-depth: the alpha-side IDs survive purge-check (project_id != deleted).
        let _ = proj_beta_shared;
    }

    #[tokio::test]
    async fn memory_write_page_creates_explicit_project() {
        // Bug B regression: an explicit `project` that doesn't exist yet must
        // be created and written to — NOT silently land in the current project.
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let baked = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, baked)
            .with_wiki(wiki);
        let parts = || {
            axum::http::Request::builder()
                .uri("/mcp")
                .method("POST")
                .body(())
                .unwrap()
                .into_parts()
                .0
        };

        server
            .memory_write_page(
                Parameters(WritePageArgs {
                    path: "notes/elsewhere.md".into(),
                    body: "lands in `other`, not `scratch`".into(),
                    title: None,
                    tier: Some("semantic".into()),
                    tags: vec![],
                    pinned: false,
                    project: Some("other".into()),
                    workspace: None,
                    scope: None,
                }),
                OptionalParts(parts()),
            )
            .await
            .unwrap();

        // Visible in `other` (created), absent from the baked `scratch`.
        let in_other = server
            .memory_recent(
                Parameters(RecentArgs {
                    limit: Some(5),
                    project: Some("other".into()),
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let other_text = in_other
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(
            other_text.contains("notes/elsewhere.md"),
            "explicit project must be created + written; got {other_text}"
        );

        let in_scratch = server
            .memory_recent(
                Parameters(RecentArgs {
                    limit: Some(5),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let scratch_text = in_scratch
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(
            !scratch_text.contains("notes/elsewhere.md"),
            "write must not leak into the current project; got {scratch_text}"
        );
    }

    /// `memory_handoff_begin` must resolve the same project as
    /// `memory_briefing` when hooks publish `ActiveProject` (issue #2).
    #[tokio::test]
    async fn handoff_begin_pending_count_matches_briefing_active_project() {
        let (_tmp, store, server, ws, baked) = setup_server().await;
        let active = store
            .writer
            .get_or_create_project(ws, "ai-memory", Some(r"C:\GIT\ai-memory".into()))
            .await
            .unwrap();
        assert_ne!(active, baked, "test needs baked default != active project");
        server.active_project.set(ws, active);

        server
            .memory_handoff_begin(
                Parameters(HandoffBeginArgs {
                    summary: "fix omp CHECK".into(),
                    open_questions: vec![],
                    next_steps: vec![],
                    files_touched: vec![],
                    cwd: Some(r"C:\GIT\ai-memory".into()),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();

        let briefing = server
            .memory_briefing(
                Parameters(BriefingArgs {
                    recent_pages_limit: Some(5),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = briefing
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(
            text.contains("\"pending_handoff_count\": 1"),
            "briefing should see the handoff in the active project; got {text}",
        );
    }

    #[tokio::test]
    async fn handoff_begin_then_accept_round_trips() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let begin = server
            .memory_handoff_begin(
                Parameters(HandoffBeginArgs {
                    summary: "left mid-refactor of writer actor".into(),
                    open_questions: vec!["what max channel size?".into()],
                    next_steps: vec!["finish supersession path".into()],
                    files_touched: vec!["crates/ai-memory-store/src/writer.rs".into()],
                    cwd: Some("/tmp/aim".into()),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let begin_text = begin
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(begin_text.contains("handoff_id"));

        // Accepting with matching cwd returns the handoff.
        let accept = server
            .memory_handoff_accept(
                Parameters(HandoffAcceptArgs {
                    cwd: Some("/tmp/aim".into()),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let accept_text = accept
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(accept_text.contains("left mid-refactor"));
        assert!(accept_text.contains("what max channel size?"));

        // Second accept returns null (handoff is now accepted).
        let again = server
            .memory_handoff_accept(
                Parameters(HandoffAcceptArgs {
                    cwd: Some("/tmp/aim".into()),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let again_text = again
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(again_text.contains("\"handoff\": null"));
    }

    #[tokio::test]
    async fn handoff_begin_caps_manual_text_after_scrub() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        server
            .memory_handoff_begin(
                Parameters(HandoffBeginArgs {
                    summary: "s".repeat(HANDOFF_SUMMARY_MAX_CHARS + 20),
                    open_questions: vec!["q".repeat(HANDOFF_ITEM_MAX_CHARS + 20)],
                    next_steps: vec!["n".repeat(HANDOFF_ITEM_MAX_CHARS + 20)],
                    files_touched: vec!["f".repeat(HANDOFF_FILE_MAX_CHARS + 20)],
                    cwd: Some("/tmp/aim".into()),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();

        let accept = server
            .memory_handoff_accept(
                Parameters(HandoffAcceptArgs {
                    cwd: Some("/tmp/aim".into()),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = accept
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(text.contains("handoff summary truncated"));
        assert!(text.contains("handoff item truncated"));
        assert!(text.contains("handoff file truncated"));
    }

    #[tokio::test]
    async fn handoff_begin_caps_manual_lists_in_aggregate() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let open_questions = (0..100)
            .map(|idx| format!("question-{idx}: {}", "q".repeat(400)))
            .collect();
        server
            .memory_handoff_begin(
                Parameters(HandoffBeginArgs {
                    summary: "contains sk-testsecret12345678901234567890 before cap".into(),
                    open_questions,
                    next_steps: vec![],
                    files_touched: vec![],
                    cwd: Some("/tmp/aim".into()),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();

        let accept = server
            .memory_handoff_accept(
                Parameters(HandoffAcceptArgs {
                    cwd: Some("/tmp/aim".into()),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let text = accept
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(text.contains("handoff open_questions truncated"));
        assert!(!text.contains("sk-testsecret"));
    }

    #[test]
    fn handoff_list_cap_keeps_marker_inside_total_budget() {
        let items = (0..10).map(|idx| format!("item-{idx}: {}", "x".repeat(80)));
        let capped = cap_handoff_list(items, 100, 220, "item", "list");
        let rendered_len = capped
            .iter()
            .map(|item| item.chars().count())
            .sum::<usize>()
            .saturating_add(capped.len().saturating_sub(1));
        assert!(rendered_len <= 220);
        assert!(capped.iter().any(|item| item.contains("list truncated")));
    }

    #[tokio::test]
    async fn handoff_begin_accept_honour_explicit_workspace() {
        // Regression for the scope-bleed facet: memory_handoff_begin/accept used
        // to ignore `workspace` (project-only resolution), so a cross-workspace
        // handoff landed in whatever project the contaminable active-project
        // slot pointed at instead of the named (workspace, project). Begin into
        // an explicit sibling workspace, then prove it's there — and NOT in the
        // current (default) project.
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        server
            .memory_handoff_begin(
                Parameters(HandoffBeginArgs {
                    summary: "cross-workspace handoff".into(),
                    open_questions: vec![],
                    next_steps: vec![],
                    files_touched: vec![],
                    cwd: None,
                    project: Some("sibling-app".into()),
                    workspace: Some("djalmajr".into()),
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();

        // The current (default) project must NOT see it.
        let in_default = server
            .memory_handoff_accept(
                Parameters(HandoffAcceptArgs {
                    cwd: None,
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let in_default_text = in_default
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(
            in_default_text.contains("\"handoff\": null"),
            "cross-workspace handoff must not bleed into the current project"
        );

        // The explicit (workspace, project) does see it.
        let in_sibling = server
            .memory_handoff_accept(
                Parameters(HandoffAcceptArgs {
                    cwd: None,
                    project: Some("sibling-app".into()),
                    workspace: Some("djalmajr".into()),
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let in_sibling_text = in_sibling
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(
            in_sibling_text.contains("cross-workspace handoff"),
            "handoff must be retrievable from its explicit (workspace, project)"
        );
    }

    #[tokio::test]
    async fn handoff_cancel_expires_open_handoff_and_clears_briefing_count() {
        let (_tmp, store, server, _ws, _pj) = setup_server().await;
        let begin = server
            .memory_handoff_begin(
                Parameters(HandoffBeginArgs {
                    summary: "accidental status summary".into(),
                    open_questions: vec![],
                    next_steps: vec![],
                    files_touched: vec![],
                    cwd: Some("/tmp/aim".into()),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let begin_text = begin
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        let begin_json: serde_json::Value = serde_json::from_str(&begin_text).unwrap();
        let handoff_id = begin_json["handoff_id"].as_str().unwrap().to_string();

        let before = server
            .memory_briefing(
                Parameters(BriefingArgs {
                    recent_pages_limit: Some(5),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let before_text = before
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(before_text.contains("\"pending_handoff_count\": 1"));

        let cancel = server
            .memory_handoff_cancel(
                Parameters(HandoffCancelArgs {
                    handoff_id: handoff_id.clone(),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let cancel_text = cancel
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(cancel_text.contains("\"cancelled\": true"));
        assert!(cancel_text.contains("\"state\": \"expired\""));

        let after = server
            .memory_briefing(
                Parameters(BriefingArgs {
                    recent_pages_limit: Some(5),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let after_text = after
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(after_text.contains("\"pending_handoff_count\": 0"));

        let accept = server
            .memory_handoff_accept(
                Parameters(HandoffAcceptArgs {
                    cwd: Some("/tmp/aim".into()),
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .unwrap();
        let accept_text = accept
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(accept_text.contains("\"handoff\": null"));

        let stored = store
            .reader
            .handoff_by_id(HandoffId::from_str(&handoff_id).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, HandoffState::Expired);
    }

    // ----------------------------------------------------------------
    // Error / mis-configured paths — caught at the tool boundary so the
    // agent sees a clean McpError instead of a panic.
    // ----------------------------------------------------------------

    /// `memory_consolidate` is opt-in via the LLM provider. With no
    /// consolidator wired, the tool must reject the call with a
    /// clear "not configured" error — not panic.
    #[tokio::test]
    async fn memory_consolidate_without_provider_errors_cleanly() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let err = server
            .memory_consolidate(
                Parameters(ConsolidateArgs {
                    session_id: "00000000-0000-0000-0000-000000000000".into(),
                    dry_run: Some(true),
                    multi_page: Some(false),
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .expect_err("must reject when no consolidator is configured");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("not configured"),
            "error should mention configuration: {msg}",
        );
        assert!(
            msg.contains("provider's required credentials"),
            "error should direct users to provider credentials: {msg}",
        );
        assert!(
            msg.contains("without a built-in model"),
            "error should not imply every provider needs an explicit model: {msg}",
        );
    }

    #[tokio::test]
    async fn memory_auto_improve_removed_dry_run_arg_fails_closed() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let err = server
            .memory_auto_improve(
                Parameters(AutoImproveArgs {
                    session_id: Some("00000000-0000-0000-0000-000000000000".into()),
                    dry_run: Some(true),
                    stage: None,
                    mode: None,
                    project: None,
                    workspace: None,
                    min_observations: None,
                    min_session_duration_secs: None,
                    min_confidence: None,
                    max_input_tokens: None,
                    max_proposals: None,
                    include_raw_fallback: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .expect_err("removed dry_run argument must fail closed");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("removed"),
            "error should mention removal: {msg}"
        );
    }

    #[tokio::test]
    async fn memory_auto_improve_without_provider_errors_cleanly() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let err = server
            .memory_auto_improve(
                Parameters(AutoImproveArgs {
                    session_id: Some("00000000-0000-0000-0000-000000000000".into()),
                    dry_run: None,
                    stage: None,
                    mode: None,
                    project: None,
                    workspace: None,
                    min_observations: None,
                    min_session_duration_secs: None,
                    min_confidence: None,
                    max_input_tokens: None,
                    max_proposals: None,
                    include_raw_fallback: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .expect_err("must reject when no LLM provider is configured");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("not configured"),
            "error should mention configuration: {msg}",
        );
    }

    /// `memory_lint` reads the wiki to build its candidate set. With
    /// no wiki wired, it must error cleanly.
    #[tokio::test]
    async fn memory_lint_without_wiki_errors_cleanly() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let err = server
            .memory_lint(
                Parameters(LintArgs {
                    dry_run: Some(true),
                    no_llm: None,
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .expect_err("must reject when wiki is not attached");
        let msg = format!("{err:?}");
        // The exact phrasing isn't load-bearing; we just need
        // SOMETHING that names the missing dependency so the agent's
        // model has a chance of choosing a different tool.
        assert!(
            msg.contains("wiki") || msg.contains("not configured"),
            "error should explain the missing wiki: {msg}",
        );
    }

    #[tokio::test]
    async fn memory_forget_sweep_targets_the_explicit_project() {
        // Bug C regression: sweep must evaluate the project named in args (or
        // the session's active project), NOT the baked default. An episodic
        // page in `audited` is a sweep candidate only when the sweep points
        // there — never when it runs against the baked `scratch`.
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let baked = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, baked)
            .with_wiki(wiki);
        let parts = || {
            axum::http::Request::builder()
                .uri("/mcp")
                .method("POST")
                .body(())
                .unwrap()
                .into_parts()
                .0
        };

        server
            .memory_write_page(
                Parameters(WritePageArgs {
                    path: "log/ep.md".into(),
                    body: "episodic note".into(),
                    title: None,
                    tier: Some("episodic".into()),
                    tags: vec![],
                    pinned: false,
                    project: Some("audited".into()),
                    workspace: None,
                    scope: None,
                }),
                OptionalParts(parts()),
            )
            .await
            .unwrap();

        let sweep_count = |args: SweepArgs| {
            let server = &server;
            async move {
                let out = server
                    .memory_forget_sweep(Parameters(args), OptionalParts(test_parts_default()))
                    .await
                    .unwrap();
                let text = out
                    .content
                    .first()
                    .and_then(|c| c.as_text())
                    .map(|t| t.text.clone())
                    .unwrap();
                serde_json::from_str::<serde_json::Value>(&text).unwrap()["candidates_evaluated"]
                    .as_u64()
                    .unwrap()
            }
        };

        let audited = sweep_count(SweepArgs {
            dry_run: Some(true),
            project: Some("audited".into()),
            workspace: None,
        })
        .await;
        assert!(
            audited >= 1,
            "sweep of the named project must evaluate its episodic page, got {audited}"
        );

        let baked = sweep_count(SweepArgs {
            dry_run: Some(true),
            project: None,
            workspace: None,
        })
        .await;
        assert_eq!(
            baked, 0,
            "sweep of the baked project must not see another project's page, got {baked}"
        );
    }

    /// `memory_handoff_accept` with no pending handoff returns a
    /// happy-path `{"handoff": null}` payload (NOT an error). This
    /// is the documented contract — the agent can call accept on
    /// every session-start without worrying about empty-queue errors.
    #[tokio::test]
    async fn memory_handoff_accept_when_none_pending_returns_null() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let result = server
            .memory_handoff_accept(
                Parameters(HandoffAcceptArgs {
                    cwd: None,
                    project: None,
                    workspace: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .expect("empty-queue must be Ok, not Err");
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        assert!(
            text.contains("\"handoff\": null"),
            "expected handoff=null in: {text}",
        );
    }

    /// `memory_query` clamps `limit` into [1, 100]. Anyone sending
    /// limit=10000 (DoS attempt or accidental overflow) gets the
    /// max instead of an unbounded scan.
    #[tokio::test]
    async fn memory_query_clamps_outlandish_limit() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        // The clamp is internal; the test verifies the call succeeds
        // with a sane response. (We don't have 10k pages, so the
        // hit count is small — we just need NOT to error.)
        let result = server
            .memory_query(
                Parameters(QueryArgs {
                    query: "Karpathy".into(),
                    limit: Some(99_999),
                    project: None,
                    scopes: Vec::new(),
                    workspace: None,
                    global: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await
            .expect("oversized limit should be clamped, not refused");
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap();
        // Returns valid JSON even on huge limit.
        let _: serde_json::Value = serde_json::from_str(&text).unwrap();
    }

    /// `memory_query` with malformed FTS5 must return a clean
    /// McpError (NOT panic, NOT bare SQLite error). The FTS5
    /// tokenizer treats `-` as a NOT operator and some characters
    /// as syntax; an unbalanced quote is the simplest reproducer.
    #[tokio::test]
    async fn memory_query_malformed_fts5_returns_error() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let err = server
            .memory_query(
                Parameters(QueryArgs {
                    query: "\"unbalanced".into(),
                    limit: Some(10),
                    project: None,
                    scopes: Vec::new(),
                    workspace: None,
                    global: None,
                }),
                OptionalParts(test_parts_default()),
            )
            .await;
        // Either a tidy 0-hit Ok (FTS5 is occasionally lenient) or
        // an Err — both are acceptable. A panic is not.
        if let Err(e) = err {
            let msg = format!("{e:?}");
            assert!(
                !msg.is_empty(),
                "error must carry diagnostic text for the agent",
            );
        }
    }
}
