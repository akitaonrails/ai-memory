//! Pure, zero-LLM entropy / boilerplate pre-filter for A4
//! (docs/design-memory-aging.md §A4).
//!
//! Before a consolidation pass reads a page or observation, this gate answers a
//! cheap question: does this text carry enough information to be worth
//! consolidating, or is it near-empty, whitespace, or a highly-repetitive
//! boilerplate blob? Low-information input is *skipped* from the pass — never
//! deleted (invariant #16, advisory only). Skipping it keeps noise out of every
//! downstream step (the LLM prompt, the dedup clustering) at no provider cost.
//!
//! The gate is a pure function of the text plus a small config (invariant #13,
//! zero-LLM). It is **off by default**: an unconfigured [`EntropyFilterConfig`]
//! keeps every item, so an upgrade changes no consolidation output until an
//! operator opts in. The thresholds are deliberately conservative — a terse but
//! informative note (a one-line fix with a file path and an error code) must be
//! KEPT; only genuinely low-signal text is skipped.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Why a piece of text was skipped from consolidation. Advisory: a skip means
/// "not worth a consolidation pass right now", never "delete".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Fewer non-whitespace characters than the configured floor — near-empty
    /// or whitespace-only.
    NearEmpty,
    /// Shannon entropy per character is below the configured floor — a single
    /// repeated character or a tiny alphabet carrying almost no information.
    LowEntropy,
    /// The same handful of tokens repeat — a boilerplate blob (e.g. a status
    /// banner echoed many times).
    Repetitive,
}

impl SkipReason {
    /// Stable short string for reports/logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NearEmpty => "near_empty",
            Self::LowEntropy => "low_entropy",
            Self::Repetitive => "repetitive",
        }
    }
}

/// The gate's verdict for one piece of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterVerdict {
    /// Consolidate this text as usual.
    Keep,
    /// Skip this text from consolidation, for the given reason.
    Skip(SkipReason),
}

impl FilterVerdict {
    /// `true` when the text should be skipped from consolidation.
    #[must_use]
    pub const fn is_skip(self) -> bool {
        matches!(self, Self::Skip(_))
    }
}

/// Configuration for the entropy / boilerplate gate.
///
/// **Off by default** (`enabled = false`): the gate keeps everything, so an
/// existing install's consolidation output is byte-identical until an operator
/// turns it on. The thresholds are intentionally low so that turning it on only
/// removes clear noise.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EntropyFilterConfig {
    /// Master switch. `false` (the default) makes [`classify`] return
    /// [`FilterVerdict::Keep`] for every input.
    pub enabled: bool,
    /// Minimum non-whitespace character count. Text shorter than this is
    /// [`SkipReason::NearEmpty`].
    pub min_chars: usize,
    /// Minimum Shannon entropy in bits per character. Text below this is
    /// [`SkipReason::LowEntropy`].
    pub min_entropy_bits_per_char: f64,
    /// A repetition ratio (`1 - distinct_tokens / total_tokens`) above this is
    /// [`SkipReason::Repetitive`]. Only applied once a body has at least
    /// [`Self::repetition_min_tokens`] tokens, so a legitimately terse note is
    /// never judged repetitive.
    pub max_repetition_ratio: f64,
    /// Token floor before the repetition check applies.
    pub repetition_min_tokens: usize,
}

impl Default for EntropyFilterConfig {
    fn default() -> Self {
        // Off, with conservative thresholds ready for when it is turned on.
        Self {
            enabled: false,
            min_chars: 16,
            min_entropy_bits_per_char: 2.0,
            max_repetition_ratio: 0.7,
            repetition_min_tokens: 6,
        }
    }
}

impl EntropyFilterConfig {
    /// Validate operator-supplied thresholds. Returns an error string naming the
    /// offending field, mirroring how the sweep validates its own coefficients.
    ///
    /// # Errors
    /// Returns `Err` when the entropy floor is negative or non-finite, or the
    /// repetition ratio is outside `0.0..=1.0`.
    pub fn validate(&self) -> Result<(), String> {
        if !self.min_entropy_bits_per_char.is_finite() || self.min_entropy_bits_per_char < 0.0 {
            return Err(
                "entropy_filter.min_entropy_bits_per_char must be a finite number \
                 greater than or equal to zero"
                    .to_string(),
            );
        }
        if !self.max_repetition_ratio.is_finite()
            || !(0.0..=1.0).contains(&self.max_repetition_ratio)
        {
            return Err(
                "entropy_filter.max_repetition_ratio must be between 0.0 and 1.0".to_string(),
            );
        }
        Ok(())
    }
}

/// Classify one piece of text against the gate.
///
/// Pure and deterministic. With `cfg.enabled = false` (the default) it always
/// returns [`FilterVerdict::Keep`], so the pass behaves exactly as it did before
/// the filter existed.
#[must_use]
pub fn classify(text: &str, cfg: &EntropyFilterConfig) -> FilterVerdict {
    if !cfg.enabled {
        return FilterVerdict::Keep;
    }
    // Near-empty: judge by non-whitespace characters, so a body that is mostly
    // blank lines is treated as empty rather than long.
    let non_ws: usize = text.chars().filter(|c| !c.is_whitespace()).count();
    if non_ws < cfg.min_chars {
        return FilterVerdict::Skip(SkipReason::NearEmpty);
    }
    // Low entropy: a single repeated character (`aaaa…`) or a tiny alphabet.
    if shannon_bits_per_char(text) < cfg.min_entropy_bits_per_char {
        return FilterVerdict::Skip(SkipReason::LowEntropy);
    }
    // Repetitive: the same few tokens over and over. Only meaningful once there
    // are enough tokens to distinguish repetition from a naturally short note.
    let tokens: Vec<&str> = text.split_whitespace().collect();
    if tokens.len() >= cfg.repetition_min_tokens {
        #[allow(clippy::cast_precision_loss)]
        let distinct = tokens
            .iter()
            .map(|t| t.to_ascii_lowercase())
            .collect::<std::collections::HashSet<_>>()
            .len() as f64;
        #[allow(clippy::cast_precision_loss)]
        let total = tokens.len() as f64;
        let repetition = 1.0 - distinct / total;
        if repetition > cfg.max_repetition_ratio {
            return FilterVerdict::Skip(SkipReason::Repetitive);
        }
    }
    FilterVerdict::Keep
}

/// Shannon entropy of the text in bits per character, over its character
/// distribution. `0.0` for empty text and for a single repeated character.
#[must_use]
fn shannon_bits_per_char(text: &str) -> f64 {
    let mut counts: HashMap<char, u64> = HashMap::new();
    let mut total: u64 = 0;
    for c in text.chars() {
        *counts.entry(c).or_insert(0) += 1;
        total += 1;
    }
    if total == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let total_f = total as f64;
    let mut bits = 0.0;
    for &count in counts.values() {
        #[allow(clippy::cast_precision_loss)]
        let p = count as f64 / total_f;
        bits -= p * p.log2();
    }
    bits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> EntropyFilterConfig {
        EntropyFilterConfig {
            enabled: true,
            ..EntropyFilterConfig::default()
        }
    }

    #[test]
    fn disabled_keeps_everything() {
        let cfg = EntropyFilterConfig::default();
        assert!(!cfg.enabled, "off by default");
        assert_eq!(classify("", &cfg), FilterVerdict::Keep);
        assert_eq!(
            classify("aaaaaaaaaaaaaaaaaaaaaaaa", &cfg),
            FilterVerdict::Keep
        );
        assert_eq!(
            classify("yes ".repeat(20).as_str(), &cfg),
            FilterVerdict::Keep
        );
    }

    #[test]
    fn empty_and_whitespace_are_near_empty() {
        assert_eq!(
            classify("", &on()),
            FilterVerdict::Skip(SkipReason::NearEmpty)
        );
        assert_eq!(
            classify("   \n\t   \n  ", &on()),
            FilterVerdict::Skip(SkipReason::NearEmpty)
        );
        assert_eq!(
            classify("ok", &on()),
            FilterVerdict::Skip(SkipReason::NearEmpty),
            "too short to carry information"
        );
    }

    #[test]
    fn single_repeated_character_is_low_entropy() {
        // Long enough to pass the near-empty floor, but zero information.
        assert_eq!(
            classify("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", &on()),
            FilterVerdict::Skip(SkipReason::LowEntropy)
        );
    }

    #[test]
    fn repeated_token_blob_is_repetitive() {
        // Distinct chars keep entropy up, but one token repeats — boilerplate.
        let verdict = classify("status ok status ok status ok status ok status ok", &on());
        assert_eq!(
            verdict,
            FilterVerdict::Skip(SkipReason::Repetitive),
            "{verdict:?}"
        );
    }

    #[test]
    fn terse_but_informative_fix_note_is_kept() {
        // The tuning target: a short fix note with a file path and an error
        // code must survive the gate.
        let note = "Fixed crates/ai-memory-store/src/ops.rs which raised E0433 on build";
        assert_eq!(classify(note, &on()), FilterVerdict::Keep, "{note}");
    }

    #[test]
    fn a_normal_prose_note_is_kept() {
        let note = "Set AI_MEMORY_AUTH_TOKEN before serving on a non-loopback bind, \
             and configure the allowed hosts to guard against DNS rebinding.";
        assert_eq!(classify(note, &on()), FilterVerdict::Keep, "{note}");
    }

    #[test]
    fn entropy_zero_for_empty_and_uniform() {
        assert_eq!(shannon_bits_per_char(""), 0.0);
        assert_eq!(shannon_bits_per_char("aaaa"), 0.0);
        assert!(
            shannon_bits_per_char("abcd") > 1.9,
            "4 equally likely chars = 2 bits"
        );
    }

    #[test]
    fn validate_rejects_bad_thresholds() {
        let mut cfg = on();
        cfg.min_entropy_bits_per_char = -1.0;
        assert!(cfg.validate().is_err());
        let mut cfg = on();
        cfg.max_repetition_ratio = 1.5;
        assert!(cfg.validate().is_err());
        assert!(on().validate().is_ok());
    }
}
