//! [`AiMemoryServer`] — the MCP server skeleton + tool router.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;

use ai_memory_consolidate::{Consolidator, run_lint, run_sweep};
use ai_memory_core::{
    ActiveProject, AgentKind, NewHandoff, PageId, PagePath, ProjectId, SessionId, Tier, WorkspaceId,
};
use ai_memory_llm::{Embedder, LlmProvider};
use ai_memory_store::{DecayParams, PageHit, ReaderPool, WriterHandle};
use ai_memory_wiki::{Wiki, WritePageRequest};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::{ErrorData as McpError, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

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
directory. **Do NOT pass `project` or `cwd` arguments unless the user \
explicitly references a *different* project by name** (e.g. 'what did \
we decide in the other-app project?'). Phrases like 'this project', \
'here', 'we', 'our work', 'where did we leave off' all mean the \
*current* project — call the tool with no scoping args. If the user \
asks about a handoff and the SessionStart auto-fetched block is already \
in your context, answer from it; do NOT re-call the tool to look for it \
in another project.\n\
\n\
Lifecycle hooks already capture every prompt + tool call automatically \
— you do NOT need to write routine notes by hand. When the user \
explicitly asks to remember a permanent annotation/fact/rule, write a \
durable wiki page; do not use a handoff for that. Use these tools when \
the conversation calls for them:\n\
\n\
- `memory_query` — when the user references prior work you don't \
  recognise, or asks 'have we done / discussed X', or you're about \
  to propose architecture (always check first).\n\
- `memory_recent` — at session start, or when the user asks 'what's \
  been going on lately'. Returns the N most-recent pages.\n\
- `memory_status` — when the user asks 'is ai-memory healthy' or \
  'how big is the knowledge base'. Returns lifetime counts.\n\
- `memory_briefing` — when the user wants a STRUCTURED snapshot \
  (counts + 7d/30d activity + rules + recent pages, JSON, no LLM \
  call). Use over memory_status when more detail is wanted.\n\
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
  this tool (it'll return null because handoffs are single-use).\n\
- `memory_handoff_begin` — when the user is wrapping up and you \
  want to ensure the next agent has context (the SessionEnd hook \
  also auto-captures this). Keep the summary terse (2-3 sentences); \
  put detail in open_questions + next_steps bullets.\n\
- `memory_consolidate` — when the user asks to compile session \
  observations into wiki pages. Also runs on PreCompact, and at \
  session end only when AI_MEMORY_CONSOLIDATE_ON_SESSION_END is set.\n\
- `memory_write_page` — when the user explicitly asks to remember, \
  save, or annotate durable project knowledge. This writes a wiki page; \
  do NOT use `memory_handoff_begin` for permanent annotations.\n\
- `memory_lint` — when the user asks to audit the wiki for stale \
  pages, contradictions, or rule suggestions.\n\
- `memory_forget_sweep` — when the user wants to prune old / cold \
  pages (idempotent, supports dry-run).\n\
- `memory_install_self_routing` — when the user asks to 'install \
  ai-memory routing into this project' or 'add ai-memory to \
  CLAUDE.md / AGENTS.md'. Returns the canonical routing snippet + \
  filename hints; you then use your own Write/Edit tool to land it \
  in the right rules file (Claude Code → CLAUDE.md, Codex / \
  OpenCode / Cursor / Gemini → AGENTS.md).\n\
\n\
The routing snippet this very text comes from can also be installed \
into the project's CLAUDE.md / AGENTS.md so the guidance survives \
across sessions. From the agent: ask 'install ai-memory routing'. \
From the terminal: `ai-memory install-instructions`.";

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
struct QueryResponse<T: Serialize> {
    hits: Vec<T>,
}

#[derive(Debug, Serialize)]
struct MemoryQueryResponse {
    hits: Vec<ai_memory_store::PageHit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    raw_hits: Vec<ai_memory_store::ObservationHit>,
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
    /// currently working in (resolved from recent hook activity). **Omit unless the user explicitly names a *different* project.**
    #[serde(default)]
    project: Option<String>,
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

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WritePageArgs {
    /// Relative wiki path to write, for example `notes/santander-2025.md`.
    path: String,
    /// Markdown body. Pass the durable fact/note content, not a handoff summary.
    body: String,
    /// Optional page title; otherwise derived from the first H1 or path stem.
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
    /// working in (resolved from recent hook activity). **Omit unless the user explicitly names a *different* project.**
    #[serde(default)]
    project: Option<String>,
}

#[tool_router]
impl AiMemoryServer {
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
            tool_router: Self::tool_router(),
        }
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

    /// Resolve which `(workspace_id, project_id)` a read tool should
    /// query. Precedence (matches the documented resolution chain):
    ///   1. an explicit `project` name argument (looked up read-only in
    ///      the server's workspace; ignored if no such project exists),
    ///   2. the hook-published [`ActiveProject`] (the cwd the agent is
    ///      currently working in),
    ///   3. the server's baked-in `--project` default.
    async fn effective_ids(&self, explicit_project: Option<&str>) -> (WorkspaceId, ProjectId) {
        if let Some(name) = explicit_project.map(str::trim).filter(|s| !s.is_empty())
            && let Ok(Some(pid)) = self
                .reader
                .find_project(self.workspace_id, name.to_string())
                .await
        {
            return (self.workspace_id, pid);
        }
        self.active_project
            .get()
            .unwrap_or((self.workspace_id, self.project_id))
    }

    async fn effective_ids_for_read_args(
        &self,
        explicit_workspace: Option<&str>,
        explicit_project: Option<&str>,
    ) -> Result<(WorkspaceId, ProjectId), McpError> {
        match (
            trimmed_opt(explicit_workspace),
            trimmed_opt(explicit_project),
        ) {
            (Some(workspace), Some(project)) => self.lookup_ids(workspace, project).await,
            (Some(_), None) => Err(McpError::internal_error(
                "workspace and project must be provided together",
                None,
            )),
            (None, project) => Ok(self.effective_ids(project).await),
        }
    }

    async fn lookup_ids(
        &self,
        workspace: &str,
        project: &str,
    ) -> Result<(WorkspaceId, ProjectId), McpError> {
        let workspace_id = self
            .reader
            .find_workspace(workspace.to_owned())
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .ok_or_else(|| {
                McpError::internal_error(format!("workspace '{workspace}' not found"), None)
            })?;
        let project_id = self
            .reader
            .find_project(workspace_id, project.to_owned())
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .ok_or_else(|| {
                McpError::internal_error(format!("project '{project}' not found"), None)
            })?;
        Ok((workspace_id, project_id))
    }

    async fn resolve_query_scopes(
        &self,
        scopes: &[MemoryScopeArg],
    ) -> Result<Vec<(WorkspaceId, ProjectId)>, McpError> {
        if scopes.len() > MAX_QUERY_SCOPES {
            return Err(McpError::internal_error(
                format!("at most {MAX_QUERY_SCOPES} scopes are allowed"),
                None,
            ));
        }
        let mut seen = HashSet::new();
        let mut resolved = Vec::new();
        for scope in scopes {
            let workspace = trimmed_opt(Some(&scope.workspace))
                .ok_or_else(|| McpError::internal_error("scope workspace cannot be empty", None))?;
            let project = trimmed_opt(Some(&scope.project))
                .ok_or_else(|| McpError::internal_error("scope project cannot be empty", None))?;
            let ids = self.lookup_ids(workspace, project).await?;
            if seen.insert(ids) {
                resolved.push(ids);
            }
        }
        Ok(resolved)
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
    /// contradiction pass.
    #[must_use]
    pub fn with_consolidator(mut self, wiki: Wiki, llm: Arc<dyn LlmProvider>) -> Self {
        let consolidator = Consolidator::new(
            self.reader.clone(),
            self.writer.clone(),
            wiki.clone(),
            llm.clone(),
            self.workspace_id,
            self.project_id,
        );
        self.consolidator = Some(Arc::new(consolidator));
        self.llm = Some(llm);
        self.wiki = Some(wiki);
        self
    }

    /// Variant of [`Self::with_consolidator`] that accepts a pre-built
    /// `Arc<Consolidator>`. Used when the same consolidator must be
    /// shared with another subsystem (e.g. the hook router's
    /// PreCompact branch) so both paths see the same handle.
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

    /// Search the compiled wiki via FTS5/vector/graph retrieval. Falls back
    /// to bounded raw observation search when no compiled page matches.
    #[tool(description = "Search the project's long-term memory wiki — \
        prior sessions, decisions, gotchas, architecture notes captured \
        by ai-memory across earlier runs. Call this BEFORE proposing \
        designs, BEFORE answering 'why does X work this way', and \
        whenever the user references prior work you don't recognise. \
        FTS5 + graph RRF + (when configured) vector RRF re-ranking. \
        Returns up to `limit` pages with HTML-marked snippets and a rank \
        score (lower rank = better match). Only latest page versions. \
        If compiled wiki search misses, `raw_hits` contains bounded raw \
        observation fallback matches.")]
    async fn memory_query(
        &self,
        Parameters(args): Parameters<QueryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = args.limit.unwrap_or(self.default_limit).clamp(1, 100);
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
        let hits = if args.scopes.is_empty() {
            let (ws, proj) = self
                .effective_ids_for_read_args(args.workspace.as_deref(), args.project.as_deref())
                .await?;
            self.search_project(ws, proj, &args.query, query_vec.as_deref(), limit)
                .await
        } else {
            let scopes = self.resolve_query_scopes(&args.scopes).await?;
            let mut hits_by_id: HashMap<PageId, PageHit> = HashMap::new();
            for (ws, proj) in scopes {
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
        };
        let hits = hits.map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.spawn_access_bump(hits.iter().map(|h| h.id).collect());
        // Raw-observation fallback only applies to a single resolved
        // project; for multi-scope queries there is no single (ws, proj).
        let raw_hits = if hits.is_empty() && args.scopes.is_empty() {
            let (ws, proj) = self
                .effective_ids_for_read_args(args.workspace.as_deref(), args.project.as_deref())
                .await?;
            self.reader
                .search_observations_for_project(ws, proj, query, limit)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        } else {
            Vec::new()
        };
        let response = MemoryQueryResponse { hits, raw_hits };
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
    ) -> Result<CallToolResult, McpError> {
        let limit = args.limit.unwrap_or(self.default_limit).clamp(1, 100);
        let (ws, proj) = self
            .effective_ids_for_read_args(args.workspace.as_deref(), args.project.as_deref())
            .await?;
        let hits = self
            .reader
            .recent_pages_for_project(ws, proj, limit)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.spawn_access_bump(hits.iter().map(|h| h.id).collect());
        let response = QueryResponse { hits };
        ok_json(&response)
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
    ) -> Result<CallToolResult, McpError> {
        let report = run_sweep(
            &self.reader,
            &self.writer,
            self.workspace_id,
            self.project_id,
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
    ) -> Result<CallToolResult, McpError> {
        let Some(wiki) = self.wiki.as_ref() else {
            return Err(McpError::internal_error(
                "memory_lint requires the server to be built with a wiki handle",
                None,
            ));
        };
        let report = run_lint(
            &self.reader,
            wiki,
            self.llm.as_ref(),
            self.workspace_id,
            self.project_id,
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
        SQL transaction. Off by default; requires \
        AI_MEMORY_LLM_PROVIDER + AI_MEMORY_LLM_MODEL set on the server. \
        Pass dry_run=true to preview without writing.")]
    async fn memory_consolidate(
        &self,
        Parameters(args): Parameters<ConsolidateArgs>,
    ) -> Result<CallToolResult, McpError> {
        let Some(consolidator) = self.consolidator.as_ref() else {
            return Err(McpError::internal_error(
                "memory_consolidate not configured (set AI_MEMORY_LLM_PROVIDER + AI_MEMORY_LLM_MODEL)",
                None,
            ));
        };
        let session_id = SessionId::from_str(&args.session_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let dry = args.dry_run.unwrap_or(false);
        if args.multi_page.unwrap_or(false) {
            let outcomes = consolidator
                .consolidate_session_multi(session_id, dry)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            ok_json(&serde_json::json!({ "outcomes": outcomes }))
        } else {
            let outcome = consolidator
                .consolidate_session(session_id, dry)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            ok_json(&outcome)
        }
    }

    /// Write or update a durable wiki page.
    #[tool(description = "Write or update a durable wiki page for the \
        current project. Use this when the user explicitly asks to \
        remember, save, pin, annotate, or make permanent a fact/rule/note. \
        This is for long-lived project knowledge; do NOT use \
        memory_handoff_begin for permanent annotations. Choose a stable \
        relative path such as `notes/<topic>.md`, `concepts/<topic>.md`, \
        `decisions/<topic>.md`, or `_rules/<topic>.md`. `tier` defaults \
        to `semantic`; set `pinned=true` for facts that should never decay.")]
    async fn memory_write_page(
        &self,
        Parameters(args): Parameters<WritePageArgs>,
    ) -> Result<CallToolResult, McpError> {
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
        let (ws, proj) = self.effective_ids(args.project.as_deref()).await;

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
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        ok_json(&serde_json::json!({
            "page_id": page_id.to_string(),
            "path": path.to_string()
        }))
    }

    /// Create a handoff snapshot for the next agent CLI.
    #[tool(description = "Record a cross-agent handoff snapshot for the \
        NEXT agent that opens this project (e.g. Codex picking up after \
        Claude Code). The next session's SessionStart hook automatically \
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
    ) -> Result<CallToolResult, McpError> {
        // Handoffs bypass `Wiki::write_page` (they live in their own
        // table), so scrub the agent-supplied free-text here. We don't
        // touch `cwd` or `files_touched` — they're path lists that the
        // path-pattern regexes already cover when applicable, but we
        // pass each entry through anyway as defence-in-depth.
        let s = &self.sanitizer;
        let (ws, proj) = self.effective_ids(args.project.as_deref()).await;
        let handoff = NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::Other,
            to_agent: None,
            cwd: args.cwd.map(std::path::PathBuf::from),
            summary: s.scrub(&args.summary),
            open_questions: args.open_questions.iter().map(|q| s.scrub(q)).collect(),
            next_steps: args.next_steps.iter().map(|n| s.scrub(n)).collect(),
            files_touched: args.files_touched.iter().map(|f| s.scrub(f)).collect(),
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
    ) -> Result<CallToolResult, McpError> {
        let (ws, proj) = self.effective_ids(args.project.as_deref()).await;
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

    /// Report aggregate counts (pages, sessions, observations).
    #[tool(description = "Report aggregate memory counts and runtime status \
        (pages latest, pages all versions, sessions, observations). \
        Use this at session start to see how much context the agent has \
        accumulated for this workspace.")]
    async fn memory_status(
        &self,
        Parameters(args): Parameters<StatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (ws, proj) = self
            .effective_ids_for_read_args(args.workspace.as_deref(), args.project.as_deref())
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
        deterministic. Use this when you want a programmatic view of \
        project state; use `memory_explore` if you want an LLM-composed \
        prose summary on top of the same data.")]
    async fn memory_briefing(
        &self,
        Parameters(args): Parameters<BriefingArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = args.recent_pages_limit.unwrap_or(10);
        let (ws, proj) = self
            .effective_ids_for_read_args(args.workspace.as_deref(), args.project.as_deref())
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
    ) -> Result<CallToolResult, McpError> {
        let limit = args.recent_pages_limit.unwrap_or(10);
        let (ws, proj) = self
            .effective_ids_for_read_args(args.workspace.as_deref(), args.project.as_deref())
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
    #[tool(description = "Returns the canonical ai-memory routing snippet \
        (the markdown block that tells the agent WHEN to call \
        memory_query / memory_recent / memory_handoff_accept / etc.) \
        plus filename hints per agent. Use when the user asks 'install \
        ai-memory routing in this project' or 'add ai-memory to \
        CLAUDE.md'. After calling, use your Write/Edit tool to: (a) \
        pick the right rules file for yourself — Claude Code → \
        CLAUDE.md, Codex / OpenCode / Cursor / Gemini CLI → AGENTS.md \
        (or check `agent_filenames` in the response); (b) if the file \
        already has a block bracketed by `<!-- ai-memory:start -->` / \
        `<!-- ai-memory:end -->`, replace that block in place; \
        otherwise append `markered_block` to the file with one blank \
        line of separation. The block IS idempotent — re-runs replace \
        in place. This tool is the source of truth for the snippet; \
        do NOT improvise the routing table from memory.")]
    async fn memory_install_self_routing(&self) -> Result<CallToolResult, McpError> {
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
                "default": "AGENTS.md"
            },
            "notes": [
                "Pick the filename matching your own agent identity.",
                "If the target file already contains <!-- ai-memory:start --> / <!-- ai-memory:end -->, replace ONLY that block in place; preserve every other line.",
                "If the file doesn't exist, create it with just the markered_block (plus a trailing newline).",
                "If the file exists but has no ai-memory markers, append the markered_block with one blank line of separation from existing content."
            ]
        });
        ok_json(&response)
    }
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

fn ok_json<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let s = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(s)]))
}

fn trimmed_opt(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{NewObservation, NewPage, NewSession, ObservationKind, PagePath, Tier};
    use ai_memory_store::Store;
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
            })
            .await
            .unwrap();

        let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj);
        (tmp, store, server, ws, proj)
    }

    #[tokio::test]
    async fn server_constructs_with_tool_router() {
        let (_tmp, _store, _server, _ws, _pj) = setup_server().await;
    }

    #[test]
    fn prompts_cover_every_mcp_tool() {
        let tools = [
            "memory_query",
            "memory_recent",
            "memory_status",
            "memory_briefing",
            "memory_explore",
            "memory_handoff_accept",
            "memory_handoff_begin",
            "memory_consolidate",
            "memory_write_page",
            "memory_lint",
            "memory_forget_sweep",
            "memory_install_self_routing",
        ];
        let routing = ai_memory_core::full_block();
        for tool in tools {
            assert!(
                MEMORY_INSTRUCTIONS.contains(tool),
                "MCP handshake instructions omit {tool}"
            );
            assert!(
                routing.contains(tool),
                "routing snippet omit {tool}; update ai_memory_core::SNIPPET_BODY"
            );
        }
    }

    #[test]
    fn prompts_route_permanent_annotations_to_write_page_not_handoff() {
        for prompt in [MEMORY_INSTRUCTIONS, ai_memory_core::SNIPPET_BODY] {
            assert!(
                prompt.contains("permanent") || prompt.contains("permanently"),
                "prompt must mention permanent memory use cases"
            );
            assert!(
                prompt.contains("memory_write_page"),
                "prompt must expose memory_write_page"
            );
            assert!(
                prompt.contains("do NOT use") || prompt.contains("do **not** use"),
                "prompt must explicitly disallow handoffs for permanent notes"
            );
        }
    }

    /// Read tools resolve the project in the order: explicit `project`
    /// arg → hook-published active project → baked-in default (issue #2).
    #[tokio::test]
    async fn effective_ids_follows_precedence_chain() {
        let (_tmp, store, server, ws, baked) = setup_server().await;

        // Baseline: nothing published, no arg → baked-in default.
        assert_eq!(server.effective_ids(None).await, (ws, baked));

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
        assert_eq!(server.effective_ids(None).await, (ws, other));

        // An explicit (existing) project arg wins over the active pointer.
        assert_eq!(
            server.effective_ids(Some("scratch")).await,
            (ws, baked),
            "explicit project arg should override the active pointer"
        );

        // An explicit but unknown project name falls through to the
        // active pointer rather than erroring or returning a bogus id.
        assert_eq!(
            server.effective_ids(Some("does-not-exist")).await,
            (ws, other),
            "unknown explicit project falls through to the active pointer"
        );
    }

    #[tokio::test]
    async fn memory_query_returns_hits_via_tool_method() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let result = server
            .memory_query(Parameters(QueryArgs {
                query: "karpathy".into(),
                limit: Some(5),
                project: None,
                scopes: Vec::new(),
                workspace: None,
            }))
            .await
            .unwrap();
        let text = match result.content.first().and_then(|c| c.as_text()) {
            Some(t) => t.text.clone(),
            None => panic!("expected text content"),
        };
        assert!(text.contains("foo.md"), "expected hit; got {text}");
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
            .insert_observation(NewObservation {
                session_id,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "raw prompt".into(),
                body: "raw fallback contains quokka only detail".into(),
                importance: 5,
            })
            .await
            .unwrap();

        let result = server
            .memory_query(Parameters(QueryArgs {
                query: "quokka".into(),
                limit: Some(5),
                project: None,
                scopes: Vec::new(),
                workspace: None,
            }))
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
            })
            .await
            .unwrap();

        let result = server
            .memory_query(Parameters(QueryArgs {
                query: "workspace_specific_token".into(),
                limit: Some(5),
                project: Some("unit-testing".into()),
                scopes: Vec::new(),
                workspace: Some("practice".into()),
            }))
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
            })
            .await
            .unwrap();

        let result = server
            .memory_query(Parameters(QueryArgs {
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
            }))
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
    async fn memory_status_returns_counts() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let result = server
            .memory_status(Parameters(StatusArgs {
                project: None,
                workspace: None,
            }))
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
            .memory_briefing(Parameters(BriefingArgs {
                recent_pages_limit: Some(5),
                project: None,
                workspace: None,
            }))
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
            .memory_explore(Parameters(ExploreArgs {
                focus: None,
                recent_pages_limit: Some(5),
                project: None,
                workspace: None,
            }))
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
            .memory_recent(Parameters(RecentArgs {
                limit: Some(5),
                project: None,
                workspace: None,
            }))
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

        let result = server
            .memory_write_page(Parameters(WritePageArgs {
                path: "notes/santander-2025.md".into(),
                body: "# Santander 2025\n\nDurable tax annotation.".into(),
                title: Some("Santander 2025".into()),
                tier: Some("semantic".into()),
                tags: vec!["finance".into()],
                pinned: true,
                project: None,
            }))
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
            .memory_recent(Parameters(RecentArgs {
                limit: Some(5),
                project: None,
                workspace: None,
            }))
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
            .memory_handoff_begin(Parameters(HandoffBeginArgs {
                summary: "fix omp CHECK".into(),
                open_questions: vec![],
                next_steps: vec![],
                files_touched: vec![],
                cwd: Some(r"C:\GIT\ai-memory".into()),
                project: None,
            }))
            .await
            .unwrap();

        let briefing = server
            .memory_briefing(Parameters(BriefingArgs {
                recent_pages_limit: Some(5),
                project: None,
                workspace: None,
            }))
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
            .memory_handoff_begin(Parameters(HandoffBeginArgs {
                summary: "left mid-refactor of writer actor".into(),
                open_questions: vec!["what max channel size?".into()],
                next_steps: vec!["finish supersession path".into()],
                files_touched: vec!["crates/ai-memory-store/src/writer.rs".into()],
                cwd: Some("/tmp/aim".into()),
                project: None,
            }))
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
            .memory_handoff_accept(Parameters(HandoffAcceptArgs {
                cwd: Some("/tmp/aim".into()),
                project: None,
            }))
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
            .memory_handoff_accept(Parameters(HandoffAcceptArgs {
                cwd: Some("/tmp/aim".into()),
                project: None,
            }))
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
            .memory_consolidate(Parameters(ConsolidateArgs {
                session_id: "00000000-0000-0000-0000-000000000000".into(),
                dry_run: Some(true),
                multi_page: Some(false),
            }))
            .await
            .expect_err("must reject when no consolidator is configured");
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
            .memory_lint(Parameters(LintArgs {
                dry_run: Some(true),
                no_llm: None,
            }))
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

    /// `memory_handoff_accept` with no pending handoff returns a
    /// happy-path `{"handoff": null}` payload (NOT an error). This
    /// is the documented contract — the agent can call accept on
    /// every session-start without worrying about empty-queue errors.
    #[tokio::test]
    async fn memory_handoff_accept_when_none_pending_returns_null() {
        let (_tmp, _store, server, _ws, _pj) = setup_server().await;
        let result = server
            .memory_handoff_accept(Parameters(HandoffAcceptArgs {
                cwd: None,
                project: None,
            }))
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
            .memory_query(Parameters(QueryArgs {
                query: "Karpathy".into(),
                limit: Some(99_999),
                project: None,
                scopes: Vec::new(),
                workspace: None,
            }))
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
            .memory_query(Parameters(QueryArgs {
                query: "\"unbalanced".into(),
                limit: Some(10),
                project: None,
                scopes: Vec::new(),
                workspace: None,
            }))
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
