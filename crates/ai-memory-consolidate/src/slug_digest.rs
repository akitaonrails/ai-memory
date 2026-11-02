//! Digest suffixes that keep a title which loses letters or digits to an
//! ASCII slug fold from sharing one path with a different title (#1943).
//!
//! A fold that keeps only `[a-z0-9]` maps every script it cannot carry
//! onto the same separator. Two rules that share one ASCII word (`ACME`,
//! `start`) and nothing else then slug to the same `_rules/<slug>.md`,
//! and each consolidation of the later one supersedes the earlier page.
//! The `rule-<hex>` fallback only fires when a fold leaves *nothing*;
//! these helpers extend the same mechanism to the mixed-script case,
//! where the fold keeps the ASCII words and drops the rest.

use icu_normalizer::DecomposingNormalizer;
use sha2::{Digest, Sha256};

/// A combining mark NFD decomposition leaves behind (the Combining
/// Diacritical Marks block). Dropping one is the Latin diacritic fold
/// (#886), not a lost letter: `e` + U+0301 is `é`, not two characters.
pub(crate) fn is_combining_mark(c: char) -> bool {
    ('\u{0300}'..='\u{036F}').contains(&c)
}

/// A letter or digit the ASCII fold cannot carry (CJK, Cyrillic, Greek,
/// the non-decomposable Latin-1 letters ß, ø, æ, ...). Losing one must
/// cost the title its exclusive slug (#1943).
pub(crate) fn is_fold_lost_alnum(c: char) -> bool {
    !is_combining_mark(c) && c.is_alphanumeric() && !c.is_ascii_alphanumeric()
}

/// A title's identity for digest suffixes: NFD-decomposed words
/// (Unicode alphanumerics plus the combining marks riding with them),
/// lower-cased and joined with `-`. The decomposition is why composed and
/// decomposed spellings of one title share a digest, and the Unicode-wide
/// word split is why two titles that keep the same ASCII words but differ
/// elsewhere do not.
///
/// Returns `None` when no word carries a letter or digit, i.e. the title
/// has nothing to name a digest by.
pub(crate) fn title_digest_key(title: &str) -> Option<String> {
    let decomposed = DecomposingNormalizer::new_nfd().normalize(title);
    let in_word = |c: char| c.is_alphanumeric() || is_combining_mark(c);
    let key = decomposed
        .split(|c: char| !in_word(c))
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join("-");
    key.chars().any(char::is_alphanumeric).then_some(key)
}

/// First 8 hex chars of the SHA-256 of `key`: short enough to read in a
/// path, long enough that two titled pages do not collide by accident.
pub(crate) fn digest_hex8(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two titles that fold to the same ASCII words keep distinct keys,
    /// and one title's key is stable across case, spacing and composition
    /// variants, the same restatement identity the ASCII slug has.
    #[test]
    fn title_digest_keys_name_titles_apart() {
        let report = title_digest_key("ACME 客户周报的格式与范围");
        let other = title_digest_key("ACME 其他报告");
        assert_ne!(report, other);
        assert_eq!(
            report.as_deref(),
            title_digest_key("acme 客户周报的格式与范围！").as_deref()
        );
        // `é` composed and decomposed are one title.
        assert_eq!(
            title_digest_key("Café 中文"),
            title_digest_key("Cafe\u{0301} 中文"),
        );
        // Nothing to name a digest by.
        assert_eq!(title_digest_key("!!!"), None);
        assert_eq!(title_digest_key(""), None);
    }

    #[test]
    fn digest_hex8_is_eight_lowercase_hex_chars() {
        let hex = digest_hex8("acme-客户周报");
        assert_eq!(hex.len(), 8);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(hex, digest_hex8("acme-客户周报"));
    }
}
