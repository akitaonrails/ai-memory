//! `[chunking]` settings: the opt-in switch for chunked session processing
//! (`docs/design-session-chunking.md`).
//!
//! Pure data and validation. Nothing here reads the environment or the
//! filesystem; the CLI loader is the single config-read path.

use serde::{Deserialize, Serialize};

/// Smallest `max_input_tokens` accepted while chunking is enabled: the compact
/// intermediate prompts still carry instructions, schema and delimiters, and
/// below this nothing is left for evidence.
pub const MIN_CHUNKING_MAX_INPUT_TOKENS: usize = 2048;

/// Default approximate input budget for one chunked call.
pub const DEFAULT_CHUNKING_MAX_INPUT_TOKENS: usize = 8192;

/// Default ceiling on provider call attempts in one logical run.
pub const DEFAULT_CHUNKING_MAX_CALLS_PER_RUN: u32 = 128;

/// `[chunking]` table. Off by default; the other keys are inert until
/// `enabled` is `true` and are only validated then, so a stale value cannot
/// break an install that never opted in.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct ChunkingSettings {
    /// Activate chunked session processing. Never implied by session size,
    /// timeouts or provider errors.
    pub enabled: bool,
    /// Approximate token budget of one chunked call (instructions, schema and
    /// context included). Independent of `[consolidation]` budgets.
    pub max_input_tokens: usize,
    /// Cap on provider call attempts (map, reduce and retries) per run.
    pub max_calls_per_run: u32,
}

impl Default for ChunkingSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            max_input_tokens: DEFAULT_CHUNKING_MAX_INPUT_TOKENS,
            max_calls_per_run: DEFAULT_CHUNKING_MAX_CALLS_PER_RUN,
        }
    }
}

impl ChunkingSettings {
    /// Validate the activated settings. A disabled table always passes.
    ///
    /// # Errors
    ///
    /// A message naming the offending `chunking.*` key.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.max_input_tokens < MIN_CHUNKING_MAX_INPUT_TOKENS {
            return Err(format!(
                "chunking.max_input_tokens must be at least {MIN_CHUNKING_MAX_INPUT_TOKENS} \
                 (got {}); below that the compact prompts leave no room for evidence",
                self.max_input_tokens
            ));
        }
        if self.max_calls_per_run == 0 {
            return Err(
                "chunking.max_calls_per_run must be greater than zero; a zero budget \
                 would stop every run before its first call"
                    .to_string(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_settings_skip_validation() {
        let s = ChunkingSettings {
            enabled: false,
            max_input_tokens: 0,
            max_calls_per_run: 0,
        };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn enabled_rejects_a_small_budget_or_zero_calls() {
        let on = ChunkingSettings {
            enabled: true,
            ..ChunkingSettings::default()
        };
        assert!(on.validate().is_ok());
        let floor = ChunkingSettings {
            max_input_tokens: MIN_CHUNKING_MAX_INPUT_TOKENS,
            ..on
        };
        assert!(floor.validate().is_ok());
        let small = ChunkingSettings {
            max_input_tokens: MIN_CHUNKING_MAX_INPUT_TOKENS - 1,
            ..on
        };
        assert!(
            small
                .validate()
                .unwrap_err()
                .contains("chunking.max_input_tokens")
        );
        let no_calls = ChunkingSettings {
            max_calls_per_run: 0,
            ..on
        };
        assert!(
            no_calls
                .validate()
                .unwrap_err()
                .contains("chunking.max_calls_per_run")
        );
    }
}
