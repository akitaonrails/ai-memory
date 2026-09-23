//! Pure, zero-LLM keep-token miner for A2 extractive tier-down
//! (docs/design-memory-aging.md §A2).
//!
//! When the forget sweep tiers a cold episodic page DOWN instead of evicting
//! it, the prose body is dropped but the *durable facts* must survive. This
//! module mines those facts with regexes only — no provider, no network
//! (invariant #13, zero-LLM). The hypothesis A2 tests is that the identifiers,
//! file paths, URLs, code spans and error codes in a session note are what a
//! later search actually needs; the surrounding prose is the expensive,
//! low-signal part.
//!
//! The patterns are compiled once via [`std::sync::LazyLock`] and reused, the
//! same shape the core sanitizer uses for its built-in patterns. The function
//! is pure and bounded (deduped, capped count and length) so a pathological
//! body cannot blow up the compacted page.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

/// Maximum distinct keep-tokens mined from one body. A compacted page is a
/// terse residue, not a second copy of the body; the cap keeps it that way.
const MAX_TOKENS: usize = 48;
/// Tokens longer than this are almost certainly a run-on match, not a fact.
const MAX_TOKEN_LEN: usize = 128;
/// Below this a token carries no signal (single characters, stray punctuation).
const MIN_TOKEN_LEN: usize = 2;

/// One ordered set of pattern + capture-group, evaluated over the whole body.
struct Pattern {
    re: Regex,
    /// Capture group whose text is the token (0 = the whole match).
    group: usize,
}

/// The mined token classes, in the order they are appended (higher-signal
/// classes first, so the cap keeps the most useful tokens when a body overflows
/// it).
static PATTERNS: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    let raw: &[(&str, usize)] = &[
        // URLs (http/https). Trailing sentence punctuation is trimmed below.
        (r#"https?://[^\s<>()\[\]"'`]+"#, 0),
        // File paths: a slash-separated path ending in an extension …
        (r"\b(?:[A-Za-z0-9_.-]+/)+[A-Za-z0-9_.-]+\.[A-Za-z0-9]+\b", 0),
        // … or a bare filename with a recognised source/config extension.
        (
            r"\b[A-Za-z0-9_.-]+\.(?:rs|md|toml|sql|json|ya?ml|sh|py|js|ts|tsx|jsx|rb|go|c|h|cc|cpp|hpp|txt|lock|cfg|ini|env|xml|html|css|proto)\b",
            0,
        ),
        // Rust-style error codes (E0433) and HTTP status codes (HTTP 500).
        (r"\bE\d{2,4}\b", 0),
        (r"\bHTTP\s?\d{3}\b", 0),
        // Inline code spans: keep the inner text, not the backticks.
        (r"`([^`\n]{1,120})`", 1),
        // UPPER_SNAKE_CASE constants and env-var names.
        (r"\b[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+\b", 0),
        // Long identifiers: snake_case (has `_`) or camelCase (lower→Upper).
        (
            r"\b(?:[a-z][a-z0-9]*_[a-z0-9_]+|[a-z][a-z0-9]*[A-Z][A-Za-z0-9]*)\b",
            0,
        ),
    ];
    raw.iter()
        .map(|(p, group)| Pattern {
            re: Regex::new(p).expect("keep-token pattern must compile"),
            group: *group,
        })
        .collect()
});

/// Extract the durable keep-token set from a page body.
///
/// Deterministic, order-preserving (first occurrence wins), deduped, and
/// bounded in both count and per-token length. Returns an empty vector for pure
/// prose with no identifiers — which is exactly the signal that such a body was
/// safe to drop.
#[must_use]
pub fn mine_keep_tokens(body: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for pattern in PATTERNS.iter() {
        for caps in pattern.re.captures_iter(body) {
            let Some(m) = caps.get(pattern.group) else {
                continue;
            };
            let token = trim_token(m.as_str());
            if token.len() < MIN_TOKEN_LEN || token.len() > MAX_TOKEN_LEN {
                continue;
            }
            if seen.insert(token.to_string()) {
                out.push(token.to_string());
                if out.len() >= MAX_TOKENS {
                    return out;
                }
            }
        }
    }
    out
}

/// Trim wrapping punctuation a regex greedily swallowed at a token's edges — a
/// URL at the end of a sentence, a path inside parentheses — without touching
/// the token's interior.
fn trim_token(raw: &str) -> &str {
    raw.trim().trim_matches(|c: char| {
        matches!(
            c,
            '.' | ',' | ';' | ':' | ')' | '(' | '"' | '\'' | '<' | '>' | '`'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_note_keeps_paths_error_codes_and_urls() {
        let body = "We fixed the panic in crates/ai-memory-store/src/ops.rs by \
             guarding the upsert. The compiler reported E0433 and the server \
             returned HTTP 500. See https://github.com/akitaonrails/ai-memory/issues/776 \
             for the write-up. The env var AI_MEMORY_AUTH_TOKEN was unset.";
        let tokens = mine_keep_tokens(body);
        assert!(
            tokens.contains(&"crates/ai-memory-store/src/ops.rs".to_string()),
            "file path survives: {tokens:?}"
        );
        assert!(
            tokens.contains(&"E0433".to_string()),
            "error code: {tokens:?}"
        );
        assert!(
            tokens.iter().any(|t| t == "HTTP 500" || t == "HTTP500"),
            "HTTP status survives: {tokens:?}"
        );
        assert!(
            tokens
                .iter()
                .any(|t| t.contains("github.com/akitaonrails/ai-memory")),
            "URL survives: {tokens:?}"
        );
        assert!(
            tokens.contains(&"AI_MEMORY_AUTH_TOKEN".to_string()),
            "UPPER_SNAKE constant: {tokens:?}"
        );
    }

    #[test]
    fn inline_code_span_keeps_inner_text_without_backticks() {
        let tokens = mine_keep_tokens("call `run_sweep_with_compaction` from serve");
        assert!(
            tokens.contains(&"run_sweep_with_compaction".to_string()),
            "{tokens:?}"
        );
        assert!(
            !tokens.iter().any(|t| t.contains('`')),
            "backticks are stripped: {tokens:?}"
        );
    }

    #[test]
    fn pure_prose_yields_little_or_nothing() {
        // No identifiers, paths, URLs, or codes — the case where dropping the
        // body loses nothing a search would want.
        let tokens = mine_keep_tokens(
            "We talked for a while about the plan and agreed to revisit it later \
             once everyone had time to think it over more carefully.",
        );
        assert!(
            tokens.is_empty(),
            "prose with no durable facts mines nothing: {tokens:?}"
        );
    }

    #[test]
    fn output_is_deduped_and_order_preserving() {
        let body = "ops.rs then ops.rs again, and ops.rs a third time; then lib.rs.";
        let tokens = mine_keep_tokens(body);
        assert_eq!(
            tokens.iter().filter(|t| *t == "ops.rs").count(),
            1,
            "deduped: {tokens:?}"
        );
        let ops = tokens.iter().position(|t| t == "ops.rs");
        let lib = tokens.iter().position(|t| t == "lib.rs");
        assert!(ops < lib, "first occurrence order preserved: {tokens:?}");
    }

    #[test]
    fn token_count_is_bounded() {
        let mut body = String::new();
        for i in 0..500 {
            body.push_str(&format!("file_number_{i}.rs "));
        }
        let tokens = mine_keep_tokens(&body);
        assert!(tokens.len() <= MAX_TOKENS, "count capped: {}", tokens.len());
    }
}
