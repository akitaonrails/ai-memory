//! Startup-resolved LLM providers for each workload.

use std::collections::HashMap;
use std::sync::Arc;

use crate::{LlmProvider, LlmWorkload};

/// Default provider and exclusive workload-specific overrides.
#[derive(Clone, Default)]
pub struct LlmProviders {
    /// Provider inherited by workloads without an override.
    pub default: Option<Arc<dyn LlmProvider>>,
    /// Providers selected instead of the default for specific workloads.
    pub overrides: HashMap<LlmWorkload, Arc<dyn LlmProvider>>,
}

impl LlmProviders {
    /// Return the override, or the same default provider when none exists.
    #[must_use]
    pub fn get(&self, workload: LlmWorkload) -> Option<&Arc<dyn LlmProvider>> {
        self.overrides.get(&workload).or(self.default.as_ref())
    }

    /// Whether any workload has an available provider.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.default.is_some() || !self.overrides.is_empty()
    }
}
