//! Karpathy "LLM Wiki" consolidation pipeline.
//!
//! M7a delivers the single-page variant: rewrite one
//! `sessions/<id>.md` page from raw observations via an LLM. The
//! store's sha256-equality short-circuit + supersession chain means
//! the rewrite is a *version*, not a destructive overwrite —
//! exactly the Karpathy pattern.
//!
//! M7b extends this to multi-page atomic fan-out.

pub mod auto_improve;
pub mod auto_improve_schedule;
pub mod auto_improve_telemetry;
pub mod bootstrap;
pub mod consolidator;
pub mod curator;
pub mod embed;
pub mod lint;
pub mod projection;
pub mod sweep;
pub mod types;

pub use auto_improve::{
    AutoImproveError, AutoImproveEvalConfig, AutoImproveEvidence, AutoImproveLlmResponse,
    AutoImproveProposal, AutoImproveRejectedCandidate, AutoImproveReport, AutoImproveReviewConfig,
    DEFAULT_AUTO_IMPROVE_MAX_CHANGED_CHARS_PER_PROPOSAL,
    DEFAULT_AUTO_IMPROVE_MAX_EDIT_CONTENT_CHARS, DEFAULT_AUTO_IMPROVE_MAX_EDITS_PER_PROPOSAL,
    DEFAULT_AUTO_IMPROVE_MAX_FINAL_BODY_CHARS, DEFAULT_AUTO_IMPROVE_MAX_INPUT_TOKENS,
    DEFAULT_AUTO_IMPROVE_MAX_PATCH_EDITS_PER_RUN, DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_BODY_CHARS,
    DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_PAGES, DEFAULT_AUTO_IMPROVE_MAX_PROCEDURE_PAGE_TOKENS,
    DEFAULT_AUTO_IMPROVE_MAX_PROPOSALS, DEFAULT_AUTO_IMPROVE_MAX_REJECTION_CONTEXT,
    DEFAULT_AUTO_IMPROVE_MAX_RULE_PAGE_TOKENS, DEFAULT_AUTO_IMPROVE_MIN_CONFIDENCE,
    DEFAULT_AUTO_IMPROVE_MIN_OBSERVATIONS, DEFAULT_AUTO_IMPROVE_MIN_SESSION_DURATION_SECS,
    DEFAULT_AUTO_IMPROVE_PENDING_PATH, DEFAULT_AUTO_IMPROVE_PROPOSAL_ACTOR,
    DEFAULT_AUTO_IMPROVE_REJECTION_CONTEXT_DAYS, default_auto_improve_eval_targets,
    run_auto_improve_review,
};
pub use auto_improve_schedule::{
    ScheduledAutoImproveSettings, ScheduledAutoImproveTickOutcome,
    initialize_auto_improve_scheduler_scopes, run_auto_improve_scheduler_tick,
};
pub use auto_improve_telemetry::{
    AutoImproveTelemetryFinding, AutoImproveTelemetryParams, AutoImproveTelemetryReport,
    AutoImproveTerminalRates, DEFAULT_AUTO_IMPROVE_TELEMETRY_SINCE_DAYS,
    DEFAULT_AUTO_IMPROVE_TELEMETRY_TOP_LIMIT, build_auto_improve_telemetry_report,
    render_auto_improve_telemetry_report_markdown, run_auto_improve_telemetry_report,
};
pub use bootstrap::{
    Bootstrap, BootstrapConfig, BootstrapError, BootstrapOutcome, BootstrapSource,
    DEFAULT_CHUNK_INPUT_TOKENS, ProjectNameStrategy, SourceCounts, SourceKind, collect_sources,
    derive_project_name, discover_main_repo_root, discover_repo_root, effective_chunk_budget,
    plan_bootstrap_chunks, prune_sources_to_budget,
};
pub use consolidator::{
    BATCH_SYSTEM_PROMPT, Consolidator, ConsolidatorError, ConsolidatorResult, build_batch_request,
};
pub use curator::{
    CuratorFinding, CuratorParams, CuratorReport, render_curator_report_markdown,
    run_curator_report,
};
pub use embed::{
    EmbedBackfillCounts, EmbedBackfillError, EmbedBackfillOptions, run_embedding_backfill,
};
pub use lint::{LintError, LintFinding, LintReport, run_lint};
pub use sweep::{EvictedPage, SweepError, SweepReport, run_sweep};
pub use types::{
    ConsolidatedBatch, ConsolidatedPage, ConsolidatedPageUpdate, ConsolidationOutcome, PageKind,
    SlotKind,
};
