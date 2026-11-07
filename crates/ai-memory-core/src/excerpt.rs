//! Extractive L0/L1 excerpts of a wiki page for the SessionStart brief.
//!
//! The brief leads with a short excerpt of every standing page and spends
//! what budget is left on full bodies, so it needs a cheap, deterministic
//! way to shrink a page. Everything here is string work over the frontmatter
//! and body the store already returns: no LLM, no I/O.
//!
//! L0 prefers the frontmatter `abstract:` (the page's own one-line pointer),
//! L1 prefers `summary:` (the longer operator summary). Both fall back to the
//! prose the page opens with, read across paragraphs and lists up to the clip,
//! skipping headings and the ADR metadata fields the profile digest skips. A
//! rules page often opens with a sentence about itself; reading on is what
//! puts its first rules in the excerpt.

use crate::profile::is_metadata_field;
use crate::sanitize::truncate_utf8_bytes;

/// Longest L0 excerpt, in UTF-8 bytes (ellipsis included).
pub const BRIEF_L0_MAX_BYTES: usize = 256;

/// Longest L1 excerpt, in UTF-8 bytes (ellipsis included). Matches the L1
/// line A2 compaction writes, so a compacted page's L1 is never re-clipped.
pub const BRIEF_L1_MAX_BYTES: usize = 500;

/// How much of a body the prose walk looks at. An excerpt is at most
/// [`BRIEF_L1_MAX_BYTES`] long, so the walk never needs the rest of a large
/// page, and bounding it keeps every session start cheap however big the
/// standing pages grow.
const PROSE_SCAN_MAX_BYTES: usize = 16 * 1024;

/// Prefix of the footer A2 compaction stamps on a compacted body. It is
/// chrome about the page, not the page's prose, so the walk skips it.
const COMPACTION_FOOTER_PREFIX: &str = "_Extractively compacted from a cold episodic page";

/// The L0 and L1 excerpts of one page. Either is `None` when the page has
/// no frontmatter pointer and no prose to fall back on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PageExcerpt {
    /// Short pointer: `abstract`, else `summary`, else the leading prose,
    /// clipped to [`BRIEF_L0_MAX_BYTES`].
    pub l0: Option<String>,
    /// Longer pointer: `summary`, else `abstract`, else the leading prose,
    /// clipped to [`BRIEF_L1_MAX_BYTES`].
    pub l1: Option<String>,
}

/// Build the L0/L1 excerpts of a page from its frontmatter JSON and its
/// (frontmatter-stripped) markdown body.
#[must_use]
pub fn excerpt_from_page(frontmatter: &serde_json::Value, body: &str) -> PageExcerpt {
    let abstract_ = frontmatter_nonempty(frontmatter, "abstract");
    let summary = frontmatter_nonempty(frontmatter, "summary");
    let paragraph = || leading_prose(body);
    let l0 = abstract_
        .or(summary)
        .map(str::to_owned)
        .or_else(paragraph)
        .map(|text| truncate_utf8_bytes(&collapse_whitespace(&text), BRIEF_L0_MAX_BYTES));
    let l1 = summary
        .or(abstract_)
        .map(str::to_owned)
        .or_else(paragraph)
        .map(|text| truncate_utf8_bytes(&collapse_whitespace(&text), BRIEF_L1_MAX_BYTES));
    PageExcerpt { l0, l1 }
}

/// The prose `body` opens with, whitespace collapsed to single spaces and
/// read across paragraphs and list items. Headings, lone `---` rules and ADR
/// metadata fields (`**Status:** Accepted`) are skipped wherever they sit, and
/// so is the A2 compaction footer. Only the first [`PROSE_SCAN_MAX_BYTES`]
/// are read, and reading stops once there is more than the longest clip can
/// carry.
#[must_use]
pub fn leading_prose(body: &str) -> Option<String> {
    let mut end = body.len().min(PROSE_SCAN_MAX_BYTES);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    let mut prose = String::new();
    // A fenced block can hold blank lines, so its state carries across the
    // paragraph split.
    let mut fence: Option<(char, usize)> = None;
    for raw_block in body[..end]
        .split("\n\n")
        .flat_map(|block| block.split("\n\r\n"))
    {
        let block = raw_block.trim();
        if fence.is_none() && block.starts_with(COMPACTION_FOOTER_PREFIX) {
            continue;
        }
        for raw_line in raw_block.lines() {
            let (next, in_code) = fence_step(fence, raw_line);
            fence = next;
            if in_code {
                continue;
            }
            let line = raw_line.trim();
            if line.is_empty()
                || line.starts_with('#')
                || line == "---"
                || is_metadata_field(line_head(line))
            {
                continue;
            }
            // Collapse whitespace while copying, and stop one byte past the
            // longest clip: the clip only needs to know there is more.
            let mut space = !prose.is_empty();
            for ch in line.chars() {
                if ch.is_whitespace() {
                    space = !prose.is_empty();
                    continue;
                }
                if space {
                    prose.push(' ');
                    space = false;
                }
                prose.push(ch);
                if prose.len() > BRIEF_L1_MAX_BYTES {
                    return Some(prose);
                }
            }
        }
    }
    (!prose.is_empty()).then_some(prose)
}

/// The open code fence's glyph and run length after `line`, and whether
/// `line` is a fence line or code inside one (CommonMark §4.5, as the wiki's
/// title and link scanners track it). Code is not prose: a page that opens
/// with a command block would otherwise lead its excerpt with the command.
fn fence_step(current: Option<(char, usize)>, line: &str) -> (Option<(char, usize)>, bool) {
    let in_fence = current.is_some();
    let leading_spaces = line.bytes().take_while(|&b| b == b' ').count();
    if leading_spaces > 3 {
        return (current, in_fence);
    }
    let trimmed = &line[leading_spaces..];
    let glyph = match trimmed.chars().next() {
        Some(c @ ('`' | '~')) => c,
        _ => return (current, in_fence),
    };
    let count = trimmed.chars().take_while(|&c| c == glyph).count();
    if count < 3 {
        return (current, in_fence);
    }
    let info = &trimmed[count..];
    match current {
        // A backtick fence's info string cannot contain a backtick, so
        // ```` ```code``` text ```` is a paragraph with an inline span.
        None if glyph == '`' && info.contains('`') => (None, false),
        None => (Some((glyph, count)), true),
        Some((open, len)) if open == glyph && count >= len && info.trim().is_empty() => {
            (None, true)
        }
        Some(open) => (Some(open), true),
    }
}

/// The start of a line, long enough for any metadata label: whether a line
/// is `**Status:** …` is decided by its first few words, and checking a
/// whole multi-kilobyte line would cost more than the excerpt.
fn line_head(line: &str) -> &str {
    let mut end = line.len().min(256);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

/// A frontmatter string value, trimmed, or `None` when the key is absent,
/// not a string, or blank.
#[must_use]
pub fn frontmatter_nonempty<'a>(frontmatter: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    frontmatter
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn excerpt(frontmatter: serde_json::Value, body: &str) -> PageExcerpt {
        excerpt_from_page(&frontmatter, body)
    }

    #[test]
    fn l0_prefers_abstract_and_l1_prefers_summary() {
        let e = excerpt(
            json!({"abstract": "short pointer", "summary": "longer summary"}),
            "Body prose.",
        );
        assert_eq!(e.l0.as_deref(), Some("short pointer"));
        assert_eq!(e.l1.as_deref(), Some("longer summary"));
    }

    #[test]
    fn a_lone_summary_serves_both_tiers() {
        let e = excerpt(json!({"summary": "only summary"}), "Body prose.");
        assert_eq!(e.l0.as_deref(), Some("only summary"));
        assert_eq!(e.l1.as_deref(), Some("only summary"));
    }

    #[test]
    fn a_lone_abstract_serves_both_tiers() {
        let e = excerpt(json!({"abstract": "only abstract"}), "Body prose.");
        assert_eq!(e.l0.as_deref(), Some("only abstract"));
        assert_eq!(e.l1.as_deref(), Some("only abstract"));
    }

    #[test]
    fn blank_or_non_string_keys_fall_back_to_the_body() {
        for frontmatter in [
            json!({"abstract": "", "summary": "   \n\t"}),
            json!({"abstract": 7, "summary": null}),
            serde_json::Value::Null,
            json!({}),
        ] {
            let e = excerpt(frontmatter.clone(), "Body prose.");
            assert_eq!(e.l0.as_deref(), Some("Body prose."), "{frontmatter}");
            assert_eq!(e.l1.as_deref(), Some("Body prose."), "{frontmatter}");
        }
    }

    #[test]
    fn a_page_with_nothing_to_say_has_no_excerpt() {
        for body in ["", "   \n\n  ", "# Only a heading\n", "---\n\n## Context\n"] {
            assert_eq!(excerpt(json!({}), body), PageExcerpt::default(), "{body:?}");
        }
    }

    #[test]
    fn the_walk_continues_past_a_heading_only_block() {
        let e = excerpt(json!({}), "# Title\n\nUse nextest.");
        assert_eq!(e.l0.as_deref(), Some("Use nextest."));
        assert_eq!(e.l1.as_deref(), Some("Use nextest."));
    }

    #[test]
    fn the_adr_template_yields_its_context_sentence() {
        // ai-memory's own ADR template, as the profile digest tests it.
        let body = "# Use WAL mode\n\n**Status:** accepted   \
                    <!-- proposed | accepted | superseded by [[decisions/other]] -->\n\n\
                    ## Context\n\nReaders blocked the writer under load.\n";
        let e = excerpt(json!({}), body);
        assert_eq!(
            e.l0.as_deref(),
            Some("Readers blocked the writer under load.")
        );
        assert_eq!(
            e.l1.as_deref(),
            Some("Readers blocked the writer under load.")
        );
    }

    #[test]
    fn metadata_without_a_blank_line_is_skipped_inside_the_block() {
        let body =
            "**Status:** Accepted\n**Date:** 2026-10-01\nEvery store opens SQLite in WAL mode.";
        let e = excerpt(json!({}), body);
        assert_eq!(
            e.l0.as_deref(),
            Some("Every store opens SQLite in WAL mode.")
        );
    }

    #[test]
    fn a_terse_labelled_rule_is_prose_not_metadata() {
        let e = excerpt(json!({}), "**Package manager:** pnpm");
        assert_eq!(e.l0.as_deref(), Some("**Package manager:** pnpm"));
    }

    #[test]
    fn a_wrapped_paragraph_collapses_to_one_line() {
        let e = excerpt(
            json!({}),
            "First line\n  wraps   here.\n\nSecond paragraph.",
        );
        assert_eq!(
            e.l0.as_deref(),
            Some("First line wraps here. Second paragraph.")
        );
    }

    #[test]
    fn rules_after_a_preface_reach_the_excerpt() {
        // A rules page that opens with a sentence about itself: the rules
        // are what an agent needs, so the excerpt runs on into the list.
        let body = "# Security\n\nThis page collects the security rules.\n\n\
                    - NEVER commit secrets to git\n- Rotate tokens monthly\n";
        let e = excerpt(json!({}), body);
        assert_eq!(
            e.l0.as_deref(),
            Some(
                "This page collects the security rules. \
                 - NEVER commit secrets to git - Rotate tokens monthly"
            )
        );
    }

    #[test]
    fn headings_and_metadata_inside_the_page_are_not_prose() {
        let body = "Intro sentence.\n\n## Rules\n\n**Status:** Accepted\n- keep it boring";
        let e = excerpt(json!({}), body);
        assert_eq!(e.l0.as_deref(), Some("Intro sentence. - keep it boring"));
    }

    #[test]
    fn l1_carries_more_of_a_long_page_than_l0() {
        let body = (0..60)
            .map(|i| format!("Sentence number {i}."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let e = excerpt(json!({}), &body);
        let (l0, l1) = (e.l0.unwrap(), e.l1.unwrap());
        assert!(l1.len() > l0.len() + 200, "L1 must add depth: {l0} / {l1}");
        assert!(l1.starts_with(l0.trim_end_matches('…')));
    }

    #[test]
    fn frontmatter_whitespace_collapses_too() {
        let e = excerpt(json!({"abstract": "  two\n  lines  "}), "");
        assert_eq!(e.l0.as_deref(), Some("two lines"));
    }

    #[test]
    fn clips_are_byte_bounded_with_an_ellipsis() {
        let long = "word ".repeat(200);
        let e = excerpt(json!({}), &long);
        let l0 = e.l0.unwrap();
        let l1 = e.l1.unwrap();
        assert!(l0.len() <= BRIEF_L0_MAX_BYTES && l0.ends_with('…'), "{l0}");
        assert!(l1.len() <= BRIEF_L1_MAX_BYTES && l1.ends_with('…'), "{l1}");
        assert!(l1.len() > l0.len());
    }

    #[test]
    fn clips_never_split_a_code_point() {
        let e = excerpt(json!({"abstract": "🦀".repeat(200)}), "");
        let l0 = e.l0.unwrap();
        assert!(l0.len() <= BRIEF_L0_MAX_BYTES);
        assert!(l0.trim_end_matches('…').chars().all(|c| c == '🦀'));
    }

    #[test]
    fn text_at_the_cap_is_kept_whole() {
        let exact = "a".repeat(BRIEF_L0_MAX_BYTES);
        let e = excerpt(json!({"abstract": exact.clone()}), "");
        assert_eq!(e.l0.as_deref(), Some(exact.as_str()));
    }

    #[test]
    fn a_huge_page_costs_no_more_than_its_clip() {
        // Excerpting runs on every session start, for every core page; a
        // multi-megabyte line must not be copied and collapsed in full only
        // to be cut to 500 bytes. Reading the whole line takes about half a
        // second in a debug build; the bounded walk takes microseconds, so
        // the bound below leaves slow CI runners plenty of slack.
        let body = format!("Lead.\n\n{}", "z".repeat(5_000_000));
        let started = std::time::Instant::now();
        let e = excerpt(json!({}), &body);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "{:?}",
            started.elapsed()
        );
        assert!(e.l1.unwrap().len() <= BRIEF_L1_MAX_BYTES);
    }

    #[test]
    fn leading_prose_returns_no_more_than_the_longest_clip_needs() {
        let long_word = "z".repeat(50_000);
        let prose = leading_prose(&format!("Lead.\n\n{long_word}")).unwrap();
        assert!(prose.len() <= BRIEF_L1_MAX_BYTES + 4, "{}", prose.len());
        assert!(prose.starts_with("Lead. zzz"));
    }

    #[test]
    fn fenced_code_is_not_prose() {
        // Agent-written pages often open with a command block. Its lines are
        // code, and a `# comment` inside it is not a heading either.
        let body = "```bash\n# run the suite\ncargo test --workspace\n```\n\n\
                    Run the suite before every push.\n\n~~~\nmake lint\n\n```\nstill code\n~~~\n";
        let e = excerpt(json!({}), body);
        assert_eq!(e.l0.as_deref(), Some("Run the suite before every push."));
        assert_eq!(e.l1.as_deref(), Some("Run the suite before every push."));
    }

    #[test]
    fn the_a2_footer_is_not_prose() {
        let footer_only = "_Extractively compacted from a cold episodic page (A2 tier-down). The full \
                           pre-compaction body is retained in git history._\n";
        assert_eq!(excerpt(json!({}), footer_only), PageExcerpt::default());

        let compacted = format!(
            "The writer actor owns every SQLite write.\n\n## Retained facts\n\n- `WriterHandle`\n\n{footer_only}"
        );
        let e = excerpt(json!({"compacted": true}), &compacted);
        // The retained facts are prose to the excerpt; the footer is not.
        assert_eq!(
            e.l0.as_deref(),
            Some("The writer actor owns every SQLite write. - `WriterHandle`")
        );
    }
    #[test]
    fn the_a2_footer_is_not_prose_with_crlf() {
        let body = "The writer actor owns every SQLite write.\n\n## Retained facts\n\n- `WriterHandle`\n\n_Extractively compacted from a cold episodic page (A2 tier-down). The full pre-compaction body is retained in git history._\n";
        let expected = "The writer actor owns every SQLite write. - `WriterHandle`";
        let lf = excerpt(json!({"compacted": true}), body);
        assert_eq!(lf.l0.as_deref(), Some(expected));
        assert_eq!(lf.l1.as_deref(), Some(expected));
        for body in [body.replace('\n', "\r\n"), body.replace("\n\n", "\n\r\n")] {
            let crlf = excerpt(json!({"compacted": true}), &body);
            assert_eq!(crlf, lf, "CRLF excerpts must omit the same A2 footer as LF");
        }
    }
}
