//! Extractive tier-down body rewrite for A2 (docs/design-memory-aging.md §A2).
//!
//! Given a cold episodic page's frontmatter and body, build the *compacted*
//! replacement: keep the L0 abstract (it already lives in the frontmatter, so
//! it survives untouched), an L1 first-paragraph/frontmatter summary, and the
//! L2 regex-mined keep-token set; drop the prose body. The frontmatter gains a
//! `compacted: true` mirror of the V65 `pages.compacted_at` column.
//!
//! Pure and zero-LLM (invariant #13): the whole rewrite is regex + string
//! assembly, no provider. The original full body is never destroyed — the
//! caller writes this through the wiki layer, which supersedes the prior
//! version (invariant #16), keeping it reachable in git history and the
//! supersession chain.

use crate::keep_tokens::mine_keep_tokens;

/// The footer stamped on every compacted body so a reader (human or agent) sees
/// at a glance that prose was dropped and where the full version lives.
pub(crate) const COMPACTION_FOOTER: &str = "_Extractively compacted from a cold episodic page (A2 tier-down). The full \
     pre-compaction body is retained in git history and the supersession chain, \
     and can be recovered with `restore-page`._";

/// Longest L1 summary carried into a compacted body. A summary is a pointer,
/// not the page.
const MAX_SUMMARY_LEN: usize = 500;

/// Build the compacted frontmatter + body for a page.
///
/// Returns `(frontmatter, body)` where the frontmatter is the original with a
/// `compacted: true` marker added (and its L0 `abstract:` preserved) and the
/// body is the L1 summary + L2 keep-tokens + footer, with the prose dropped.
#[must_use]
pub fn build_compacted_markdown(
    frontmatter: &serde_json::Value,
    body: &str,
) -> (serde_json::Value, String) {
    let mut fm = match frontmatter {
        serde_json::Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    fm.insert("compacted".to_string(), serde_json::Value::Bool(true));

    let summary = summary_line(&fm, body);
    let tokens = mine_keep_tokens(body);

    let mut out = String::new();
    if let Some(summary) = &summary {
        out.push_str(summary);
        out.push_str("\n\n");
    }
    if !tokens.is_empty() {
        out.push_str("## Retained facts\n\n");
        for token in &tokens {
            // Wrap in backticks so identifiers/paths render verbatim and stay
            // single FTS tokens on re-index.
            out.push_str("- `");
            out.push_str(token);
            out.push_str("`\n");
        }
        out.push('\n');
    }
    out.push_str(COMPACTION_FOOTER);
    out.push('\n');

    (serde_json::Value::Object(fm), out)
}

/// The L1 summary: prefer an explicit frontmatter `summary:` / `abstract:`
/// line, else the first non-empty, non-heading paragraph of the body. Bounded.
fn summary_line(fm: &serde_json::Map<String, serde_json::Value>, body: &str) -> Option<String> {
    for key in ["summary", "abstract"] {
        if let Some(text) = fm.get(key).and_then(serde_json::Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(truncate(trimmed));
            }
        }
    }
    for paragraph in body.split("\n\n") {
        let trimmed = paragraph.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Collapse internal newlines so a wrapped first paragraph stays one line.
        let collapsed = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
        if !collapsed.is_empty() {
            return Some(truncate(&collapsed));
        }
    }
    None
}

/// Truncate on a char boundary, appending an ellipsis when it actually cut.
fn truncate(text: &str) -> String {
    if text.len() <= MAX_SUMMARY_LEN {
        return text.to_string();
    }
    let mut end = MAX_SUMMARY_LEN;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_frontmatter_and_preserves_abstract() {
        let fm = serde_json::json!({"title": "Fix", "abstract": "One-line summary."});
        let (out_fm, _body) = build_compacted_markdown(&fm, "prose body here");
        assert_eq!(
            out_fm.get("compacted"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(
            out_fm.get("abstract").and_then(|v| v.as_str()),
            Some("One-line summary."),
            "L0 abstract survives"
        );
        assert_eq!(out_fm.get("title").and_then(|v| v.as_str()), Some("Fix"));
    }

    #[test]
    fn keeps_tokens_and_drops_prose() {
        let fm = serde_json::json!({"title": "Fix"});
        // First paragraph is the L1 summary; the prose tail (a later paragraph)
        // is dropped, though its keep-tokens are still mined from the full body.
        let body = "Fixed the flaky sweep test.\n\n\
             The root cause was in crates/ai-memory-store/src/ops.rs and produced \
             E0433. Everyone agreed it was subtle and worth writing down carefully.";
        let (_fm, out) = build_compacted_markdown(&fm, body);
        assert!(
            out.contains("crates/ai-memory-store/src/ops.rs"),
            "keep-token survives: {out}"
        );
        assert!(out.contains("E0433"), "error code survives: {out}");
        assert!(
            !out.contains("worth writing down carefully"),
            "trailing prose dropped: {out}"
        );
        assert!(out.contains("A2 tier-down"), "footer present: {out}");
    }

    #[test]
    fn prefers_frontmatter_summary_for_l1() {
        let fm = serde_json::json!({"summary": "The gist."});
        let (_fm, out) = build_compacted_markdown(&fm, "some other first paragraph");
        assert!(out.starts_with("The gist."), "{out}");
        assert!(!out.contains("some other first paragraph"), "{out}");
    }

    #[test]
    fn pure_prose_still_produces_a_valid_compacted_body() {
        let fm = serde_json::json!({"title": "Chat"});
        let (_fm, out) = build_compacted_markdown(&fm, "we talked and agreed to revisit");
        // First paragraph becomes the L1 summary; no keep-tokens section.
        assert!(out.contains("we talked and agreed to revisit"), "{out}");
        assert!(!out.contains("## Retained facts"), "{out}");
        assert!(out.contains("A2 tier-down"), "{out}");
    }
}
