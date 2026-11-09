//! Query-scoped implicit ranking signals (#1941, slice 2).
//!
//! A `(normalized query → page)` association records that a page returned by
//! a query was actually *used*: a `memory_read_page` of that hit, or a
//! `helpful` feedback signal, followed the query in the same agent session.
//! Later queries that are lexically similar to a stored association give that
//! page a bounded authority boost, scaled by the similarity between the two
//! queries — keying the boost on the query (not the page) is what keeps a
//! popular page from becoming the answer to everything.
//!
//! Everything here is pure text math: no IO, no LLM, deterministic on both
//! ends. The persistence lives in the `query_page_signals` table (V98); the
//! capture path lives in the MCP server, which sanitizes and bounds the query
//! text before it reaches the writer.

/// Upper bound on the stored normalized query text, in Unicode scalar values.
/// Queries are short; anything past this is almost certainly a pasted block
/// of prose, and the association only ever needs enough text to re-match a
/// similar future query.
pub const QUERY_SIGNAL_TEXT_MAX_CHARS: usize = 256;

/// The stored per-association signal count saturates at this many distinct
/// sessions. One association is recorded per (session, query, page), so the
/// count approximates "how many independent sessions confirmed this query →
/// page pairing"; saturating it is what bounds the read-more → boost-more
/// feedback loop the issue flags.
pub const QUERY_SIGNAL_COUNT_CAP: u32 = 4;

/// Normalize a query for storage and comparison: lowercase, collapse every
/// run of whitespace to one space, trim, and cap the length. Deliberately
/// cheap and stable — the same input must always produce the same stored row
/// key, so this may not depend on configuration or locale.
#[must_use]
pub fn normalize_query(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(QUERY_SIGNAL_TEXT_MAX_CHARS * 4));
    let mut len = 0_usize;
    let mut in_space = false;
    let push = |out: &mut String, len: &mut usize, ch: char| {
        if *len >= QUERY_SIGNAL_TEXT_MAX_CHARS {
            return;
        }
        *len += 1;
        out.push(ch);
    };
    for ch in raw.chars() {
        if ch.is_whitespace() {
            in_space = !out.is_empty();
        } else {
            if in_space {
                push(&mut out, &mut len, ' ');
                if len >= QUERY_SIGNAL_TEXT_MAX_CHARS {
                    break;
                }
                in_space = false;
            }
            // Lowercase per scalar; fine for the ASCII and CJK traffic this
            // sees, and `to_lowercase` on a whole String would also grow the
            // output past the cap unpredictably.
            for lower in ch.to_lowercase() {
                push(&mut out, &mut len, lower);
            }
        }
    }
    out
}

/// The stable association key: the first 16 bytes of SHA-256 over the
/// normalized query. 128 bits is far beyond any realistic collision space for
/// distinct queries one project issues; the hash exists only to keep
/// human-readable text out of the primary key.
#[must_use]
pub fn query_hash(normalized: &str) -> [u8; 16] {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(normalized.as_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// Split text into comparison tokens: runs of alphanumeric characters form
/// words, while every CJK character is its own token. CJK scripts do not
/// separate words with spaces, so an unspaced CJK query would otherwise
/// collapse into one giant token that matches nothing but itself;
/// per-character tokens keep similarity working for the languages the FTS
/// side already special-cases. Accented Latin letters stay inside their word:
/// as tokens of their own, `migração` and `configuração` would share three.
fn query_tokens(text: &str) -> std::collections::BTreeSet<String> {
    let mut tokens = std::collections::BTreeSet::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() && !crate::fts_query::is_cjk(ch) {
            for lower in ch.to_lowercase() {
                current.push(lower);
            }
            continue;
        }
        if !current.is_empty() {
            tokens.insert(std::mem::take(&mut current));
        }
        if ch.is_alphanumeric() {
            // CJK ideographs, kana, hangul: a token of its own.
            for lower in ch.to_lowercase() {
                tokens.insert(lower.to_string());
            }
        }
    }
    if !current.is_empty() {
        tokens.insert(current);
    }
    tokens
}

/// Dice coefficient over the two queries' token sets: `2|A∩B| / (|A|+|B|)`,
/// in `[0, 1]`. Zero when either side has no tokens or they share none. Dice
/// (rather than Jaccard) so a stored query that is a strict subset of the
/// current one — "caddy tls" vs "caddy tls config reload" — still scores
/// proportionally instead of collapsing to a small fraction.
#[must_use]
pub fn query_similarity(a: &str, b: &str) -> f64 {
    let a = query_tokens(a);
    let b = query_tokens(b);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.intersection(&b).count();
    let total = a.len() + b.len();
    (2.0 * shared as f64) / total as f64
}

/// One stored association a candidate page carries.
#[derive(Debug, Clone)]
pub struct QuerySignalRow {
    /// Sanitized, normalized query text as stored at capture time.
    pub query_text: String,
    /// How many distinct sessions recorded this association (saturating at
    /// [`QUERY_SIGNAL_COUNT_CAP`] for ranking purposes).
    pub signal_count: u32,
}

/// The authority boost a candidate page earns from its associations for the
/// current query: `weight * best(similarity * count_saturation)` over the
/// page's stored rows. `None` when there is nothing to add — no rows, or no
/// stored query shares a token with the current one.
#[must_use]
pub fn query_signal_boost(
    rows: &[QuerySignalRow],
    current_query: &str,
    weight: f64,
) -> Option<f64> {
    if rows.is_empty() || weight <= 0.0 {
        return None;
    }
    let current = normalize_query(current_query);
    let mut best = 0.0_f64;
    for row in rows {
        let sim = query_similarity(&current, &row.query_text);
        if sim <= 0.0 {
            continue;
        }
        let saturation = f64::from(row.signal_count.min(QUERY_SIGNAL_COUNT_CAP))
            / f64::from(QUERY_SIGNAL_COUNT_CAP);
        best = best.max(sim * saturation);
    }
    (best > 0.0).then_some(weight * best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_lowercases_collapses_and_trims() {
        assert_eq!(
            normalize_query("  Caddy   TLS\nConfig\tRELOAD  "),
            "caddy tls config reload"
        );
    }

    #[test]
    fn normalize_caps_the_stored_text_at_the_bound() {
        let raw = "w".repeat(QUERY_SIGNAL_TEXT_MAX_CHARS + 100);
        let normalized = normalize_query(&raw);
        assert_eq!(
            normalized.chars().count(),
            QUERY_SIGNAL_TEXT_MAX_CHARS,
            "stored query text must be bounded"
        );
    }

    #[test]
    fn normalize_keeps_interior_spacing_after_the_cap() {
        // A cap that lands mid-token must not leave a dangling half token
        // that still matches: cut text just stops growing.
        let raw: String = "a ".repeat(QUERY_SIGNAL_TEXT_MAX_CHARS);
        let normalized = normalize_query(&raw);
        assert!(normalized.chars().count() <= QUERY_SIGNAL_TEXT_MAX_CHARS);
    }

    #[test]
    fn identical_queries_score_one() {
        assert!((query_similarity("caddy tls config", "Caddy TLS config") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn disjoint_queries_score_zero() {
        assert_eq!(query_similarity("caddy tls", "postgres vacuum"), 0.0);
    }

    #[test]
    fn subset_queries_score_proportionally() {
        // 2 shared of 2 + 5 tokens: Dice = 4/7.
        let sim = query_similarity("caddy tls", "caddy tls config reload extra");
        assert!((sim - 4.0 / 7.0).abs() < 1e-9, "{sim}");
    }

    #[test]
    fn cjk_queries_share_characters() {
        assert!(query_similarity("上次 会话 配置", "上次的会话配置") > 0.0);
    }

    /// Only the scripts the FTS side splits per character (`is_cjk`) become
    /// one-character tokens; an accented Latin word stays one word, or two
    /// unrelated Portuguese queries share their `ç`, `ã` and `o`.
    #[test]
    fn accented_latin_words_are_whole_tokens() {
        assert_eq!(query_similarity("migração", "configuração"), 0.0);
        assert_eq!(query_similarity("café", "cafe"), 0.0);
        assert!((query_similarity("Configuração TLS", "configuração tls") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn empty_text_scores_zero() {
        assert_eq!(query_similarity("", "caddy"), 0.0);
        assert_eq!(query_similarity("   ", "caddy"), 0.0);
    }

    #[test]
    fn hash_is_stable_and_normalization_sensitive() {
        assert_eq!(query_hash("caddy tls"), query_hash("caddy tls"));
        assert_ne!(query_hash("caddy tls"), query_hash("caddy  tls"));
        // The server stores normalized text, so the hash is over normalized
        // bytes: same normalized input, same key.
        assert_eq!(
            query_hash(&normalize_query("  Caddy TLS ")),
            query_hash("caddy tls")
        );
    }

    #[test]
    fn boost_requires_shared_tokens_and_scales_with_similarity() {
        let rows = vec![QuerySignalRow {
            query_text: "caddy tls".to_string(),
            signal_count: QUERY_SIGNAL_COUNT_CAP,
        }];
        let identical = query_signal_boost(&rows, "caddy tls", 0.2).unwrap();
        assert!((identical - 0.2).abs() < 1e-9, "{identical}");
        let partial = query_signal_boost(&rows, "caddy tls config reload extra", 0.2).unwrap();
        assert!(partial < identical, "{partial} must be similarity-capped");
        assert_eq!(query_signal_boost(&rows, "postgres vacuum", 0.2), None);
        assert_eq!(query_signal_boost(&[], "caddy tls", 0.2), None);
    }

    #[test]
    fn boost_saturates_after_the_count_cap() {
        let one = query_signal_boost(
            &[QuerySignalRow {
                query_text: "caddy tls".to_string(),
                signal_count: 1,
            }],
            "caddy tls",
            0.2,
        )
        .unwrap();
        let capped = query_signal_boost(
            &[QuerySignalRow {
                query_text: "caddy tls".to_string(),
                signal_count: QUERY_SIGNAL_COUNT_CAP,
            }],
            "caddy tls",
            0.2,
        )
        .unwrap();
        let beyond = query_signal_boost(
            &[QuerySignalRow {
                query_text: "caddy tls".to_string(),
                signal_count: QUERY_SIGNAL_COUNT_CAP * 10,
            }],
            "caddy tls",
            0.2,
        )
        .unwrap();
        assert!(one < capped, "one session must not earn the full boost");
        assert!((capped - beyond).abs() < 1e-9, "the count must saturate");
    }

    #[test]
    fn a_zero_weight_never_boosts() {
        let rows = vec![QuerySignalRow {
            query_text: "caddy tls".to_string(),
            signal_count: QUERY_SIGNAL_COUNT_CAP,
        }];
        assert_eq!(query_signal_boost(&rows, "caddy tls", 0.0), None);
    }
}
