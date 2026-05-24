//! Karpathy "LLM Wiki" consolidation pipeline.
//!
//! M7a delivers the single-page variant: rewrite one
//! `sessions/<id>.md` page from raw observations via an LLM. The
//! store's sha256-equality short-circuit + supersession chain means
//! the rewrite is a *version*, not a destructive overwrite —
//! exactly the Karpathy pattern.
//!
//! M7b extends this to multi-page atomic fan-out.

pub mod bootstrap;
pub mod consolidator;
pub mod lint;
pub mod sweep;
pub mod types;

pub use bootstrap::{
    Bootstrap, BootstrapConfig, BootstrapError, BootstrapOutcome, BootstrapSource, SourceCounts,
    SourceKind, collect_sources, discover_repo_root, prune_to_budget,
};
pub use consolidator::{
    BATCH_SYSTEM_PROMPT, Consolidator, ConsolidatorError, ConsolidatorResult, build_batch_request,
};
pub use lint::{LintError, LintFinding, LintReport, run_lint};
pub use sweep::{EvictedPage, SweepError, SweepReport, run_sweep};
pub use types::{
    ConsolidatedBatch, ConsolidatedPage, ConsolidatedPageUpdate, ConsolidationOutcome, PageKind,
};
