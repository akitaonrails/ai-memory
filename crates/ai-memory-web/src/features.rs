//! Safe startup facts and per-request feature availability. No credentials,
//! provider endpoints, paths, usernames or raw configuration are exposed.

use crate::state::WebState;
use ai_memory_core::{AuthLevel, Capability};
use serde::Serialize;

/// Effective startup switches, copied from the one configuration read.
#[derive(Debug, Clone, Default)]
pub struct RuntimeFeatures {
    /// A provider was successfully constructed.
    pub llm_configured: bool,
    /// Session-end compilation is scheduled.
    pub consolidate_on_session_end: bool,
    /// Default for assistant-text capture; clients may opt in separately.
    pub capture_assistant_default: bool,
    /// Default for startup recovery on this installation.
    pub backfill_on_start: bool,
    /// The automatic learning scheduler is effectively enabled.
    pub learning_scheduled: bool,
    /// Proposals wait for manual approval.
    pub review_requires_approval: bool,
    /// Scheduled maintenance is enabled.
    pub maintenance_enabled: bool,
    /// Retention sweeps have a nonzero configured cadence.
    pub retention_scheduled: bool,
    /// Human-auth routes were enabled at startup.
    pub human_login: bool,
    /// Observation-evidence retrieval stream is enabled.
    pub observation_stream: bool,
}

/// One caller-visible state and its next step; safe for HTML and JSON.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct FeatureState {
    pub id: &'static str,
    pub title: &'static str,
    pub state: &'static str,
    pub detail: &'static str,
    pub next_step: &'static str,
    pub guide: &'static str,
}

/// Compute permissions each time; startup configuration alone never grants
/// access. Missing host wiring is unknown, rather than assumed disabled.
pub(crate) async fn states(
    state: &WebState,
    level: Option<AuthLevel>,
) -> ai_memory_store::StoreResult<Vec<FeatureState>> {
    let distinguishes = state
        .reader
        .distinguishes_operators(state.trusted_proxy_identity)
        .await?;
    let admin = level
        .unwrap_or(AuthLevel::Anonymous)
        .authorize(Capability::Admin, distinguishes)
        .is_ok();
    let runtime = state.runtime.as_ref();
    let mut rows = Vec::new();
    let mut push = |id, title, status, detail, next_step, guide, private| {
        rows.push(if private && !admin {
            FeatureState {
                id, title, state: "Restricted",
                detail: "Administrator access is required to inspect this installation setting.",
                next_step: "Ask the administrator to inspect this feature. The guide remains available.",
                guide,
            }
        } else {
            FeatureState { id, title, state: status, detail, next_step, guide }
        });
    };
    push(
        "knowledge",
        "Verifiable knowledge",
        "Available",
        "Page sources, declared contradictions, validity and recall explanations use the current index and your source permissions.",
        "Open a page, then inspect Knowledge evidence or arrive through a search result.",
        "features#recall-project-routing-and-memory-pages",
        false,
    );
    push(
        "capture",
        "Lifecycle capture",
        if state.ingest_metrics.is_some() {
            "Server ready; client setup unknown"
        } else {
            "Not reported"
        },
        "A server cannot verify hooks installed on every agent machine.",
        "Install the native client and hooks on each agent machine; run a captured session and inspect its events.",
        "features#capture-and-agent-integration",
        false,
    );
    push(
        "tokens",
        "Response token investigation",
        "Client reports required",
        "The server accepts bounded usage and provenance reports. Older clients and missing transcripts leave unknown data.",
        "Update each native client, then use UTC, model, harness and session filters in Token usage.",
        "features#token-accounting-and-investigation",
        false,
    );
    push(
        "themes",
        "Themes and responsive navigation",
        "Browser preference",
        "Theme selection is local to this browser; persistence depends on browser storage.",
        "Choose light, dark or system in the navigation controls.",
        "features#themes-and-responsive-navigation",
        false,
    );
    push(
        "inbox",
        "Agent inbox",
        "Project permissions apply",
        "Reading follows project access. Sending requires write access to both project coordinates and a registered recipient.",
        "Open Inbox, select the source and destination projects, then choose an agent.",
        "features#agent-inbox-and-directed-messages",
        false,
    );
    push(
        "handoffs",
        "Handoffs and recovery",
        "Project and owner permissions apply",
        "A handoff is collected explicitly by its owner. Reading the dashboard does not claim one.",
        "Use the handoff commands from the selected repository and inspect the recovery guide.",
        "features#handoffs-and-compaction-recovery",
        false,
    );
    push(
        "workstreams",
        "Managed sessions and workstreams",
        "Available to administrator",
        "The server ledger is available; launch and checkout discovery happen on the client machine.",
        "Use ai-memory run on the agent machine; inspect Workstreams for recorded activity.",
        "features#managed-sessions-and-workstreams",
        true,
    );
    push(
        "embeddings",
        "Vector retrieval",
        if state.recall.embedder.is_some() {
            "Configured"
        } else {
            "Not configured"
        },
        "Lexical retrieval remains available. A configured embedder is not proof that a provider is currently healthy.",
        if admin {
            "Configure an embedding provider, embed the project and inspect Health for failures."
        } else {
            "Ask the administrator to configure embeddings if lexical retrieval is insufficient."
        },
        "features#embeddings-and-reranking",
        false,
    );
    push(
        "reranking",
        "Search reranking",
        if state.recall.rerank.is_some() {
            "Configured"
        } else {
            "Not configured"
        },
        "This reflects the actual reranker shared by web and MCP search.",
        if admin {
            "Configure the reranker and verify a project query with its recall explanation."
        } else {
            "Use project search; ask the administrator about optional reranking."
        },
        "features#embeddings-and-reranking",
        false,
    );
    if let Some(r) = runtime {
        push(
            "consolidation",
            "Session consolidation",
            if r.consolidate_on_session_end {
                "Enabled on session end"
            } else {
                "Manual only"
            },
            if r.llm_configured {
                "An LLM provider was constructed; provider health is checked separately."
            } else {
                "No LLM provider is configured. Rule-based summaries remain available."
            },
            "Inspect consolidate_on_session_end and provider configuration, then verify a completed session.",
            "features#llm-consolidation-and-provider-fallback",
            true,
        );
        push(
            "learning",
            "Automatic learning",
            if !r.llm_configured {
                "Unavailable without LLM"
            } else if r.learning_scheduled {
                "Scheduled"
            } else {
                "Manual only"
            },
            "Effective scheduling accounts for the enabled flag, nonzero interval and batch size.",
            "Configure the LLM and the auto_improve scheduler; inspect Learning review.",
            "features#learning-and-proposal-review",
            true,
        );
        push(
            "review",
            "Proposal approval",
            if r.review_requires_approval {
                "Manual approval required"
            } else {
                "Automatic approval configured"
            },
            "This is the configured policy for validated proposals, not a count of pending proposals.",
            "Review the pending-writes audit trail; set auto_improve.require_approval to change the policy.",
            "features#learning-and-proposal-review",
            true,
        );
        push(
            "assistant",
            "Assistant-text capture default",
            if r.capture_assistant_default {
                "Default enabled"
            } else {
                "Default disabled"
            },
            "A client can opt in separately; capture exclusions still apply.",
            "Inspect capture_assistant and the installed client's capture options.",
            "features#capture-exclusions-and-assistant-text",
            true,
        );
        push(
            "backfill",
            "Startup recovery default",
            if r.backfill_on_start {
                "Default enabled"
            } else {
                "Default disabled"
            },
            "This startup setting does not attest to hooks installed on remote clients.",
            "Inspect backfill_on_start and use the recovery guide on the affected machine.",
            "features#handoffs-and-compaction-recovery",
            true,
        );
        push(
            "retention",
            "Scheduled retention",
            if r.maintenance_enabled && r.retention_scheduled {
                "Scheduled"
            } else {
                "Disabled"
            },
            "The effective maintenance switch and nonzero sweep interval control scheduling.",
            "Inspect maintenance and retention settings; make an online backup before lifecycle operations.",
            "features#backup-retention-and-deployment",
            true,
        );
        push(
            "human-login",
            "Human account login",
            if r.human_login {
                "Enabled at startup"
            } else {
                "Not enabled at startup"
            },
            "This reports the startup auth mode. Current identity and permissions are evaluated for this request.",
            "Use the authentication setup guide and administrator status to manage credentials.",
            "features#authentication-team-access-and-the-management-console",
            true,
        );
        push(
            "observation-recall",
            "Observation-evidence retrieval",
            if r.observation_stream {
                "Enabled"
            } else {
                "Disabled"
            },
            "This is the effective retrieval.observation_stream switch loaded at startup.",
            "Inspect a project query's recall trace; configure retrieval.observation_stream to change the stream.",
            "features#recall-project-routing-and-memory-pages",
            false,
        );
    } else {
        for (id, title, guide) in [
            (
                "consolidation",
                "Session consolidation",
                "features#llm-consolidation-and-provider-fallback",
            ),
            (
                "learning",
                "Automatic learning",
                "features#learning-and-proposal-review",
            ),
            (
                "review",
                "Proposal approval",
                "features#learning-and-proposal-review",
            ),
            (
                "assistant",
                "Assistant-text capture default",
                "features#capture-exclusions-and-assistant-text",
            ),
            (
                "backfill",
                "Startup recovery default",
                "features#handoffs-and-compaction-recovery",
            ),
            (
                "retention",
                "Scheduled retention",
                "features#backup-retention-and-deployment",
            ),
            (
                "human-login",
                "Human account login",
                "features#authentication-team-access-and-the-management-console",
            ),
            (
                "observation-recall",
                "Observation-evidence retrieval",
                "features#recall-project-routing-and-memory-pages",
            ),
        ] {
            push(
                id,
                title,
                "Not reported",
                "The hosting process did not provide its effective startup settings.",
                "Ask the administrator to use a server build that publishes the safe runtime snapshot.",
                guide,
                id != "observation-recall",
            );
        }
    }
    push(
        "management",
        "Management console",
        if cfg!(feature = "management-ui") {
            "Included in this build"
        } else {
            "Not included in this build"
        },
        "Administrative access is required independently of build support.",
        "Use the administrator CLI/API, or deploy a build with the management-ui feature.",
        "features#authentication-team-access-and-the-management-console",
        true,
    );
    push(
        "external-clients",
        "External agent clients",
        "Client setup unknown",
        "Remote client versions and per-project marker files cannot be inferred from this server.",
        "Run install-hooks and doctor on each client; check a captured session in the target project.",
        "features#external-clients-and-lifecycle-capture",
        false,
    );
    push(
        "capture-policy",
        "Capture exclusions",
        "Client/project configuration",
        "Each client reads its nearest marker. The server does not read remote repository files.",
        "Inspect capture.ignore_paths in that repository's .ai-memory.toml and verify excluded events stay absent.",
        "features#capture-exclusions-and-assistant-text",
        false,
    );
    push(
        "documentation",
        "Documentation and changelogs",
        "Available",
        "Guides are bundled with this server build; installation states are separate from those reference pages.",
        "Use Markdown or the authenticated docs API for agent-readable references.",
        "features#documentation-and-changelog-access-for-agents",
        false,
    );
    Ok(rows)
}
