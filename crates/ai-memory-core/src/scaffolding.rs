//! Does a candidate title look like harness scaffolding rather than something
//! a person wrote? (#484)
//!
//! Session page titles come from the first user prompt, taken verbatim. But a
//! prompt payload is whatever the harness put there, and harnesses inject:
//! IDE context blocks, a shell prompt echoed into a paste, a bare model id.
//! Measured across one instance's 330 real session pages, 6.4% carried a
//! title that was not user content, and the rate was flat month over month
//! rather than decaying — a live writer defect, not legacy data.
//!
//! Deliberately a **shape** test, not a list of known offenders. Three
//! classes observed on one corpus is a sample of that operator's harnesses,
//! not of the problem; a blocklist is wrong on the fourth harness and fails
//! silently, because a bad title just looks like a title. `transcript.rs`
//! already carries a narrower version of this idea for `<system-reminder>`
//! and `<user_info>` on the workstream path.

/// Characters that open a decorated shell prompt rather than prose.
/// Box-drawing and the powerline separators commonly echoed into a paste.
const PROMPT_DECORATION: [char; 9] = [
    '\u{250c}', // ┌
    '\u{251c}', // ├
    '\u{2514}', // └
    '\u{2500}', // ─
    '\u{2502}', // │
    '\u{256d}', // ╭
    '\u{2570}', // ╰
    '\u{279c}', // ➜
    '\u{276f}', // ❯
];

/// `true` when `candidate` reads as harness scaffolding rather than user
/// prose, and so should not become a page title.
///
/// Errs toward keeping text: a false positive silently discards a real title,
/// while a false negative only reproduces today's behaviour.
#[must_use]
pub fn looks_like_scaffolding(candidate: &str) -> bool {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return true;
    }

    // A markup/context block: `<ide_opened_file>…`, `<system-reminder>…`,
    // `<user_info>…`. Requires a closing bracket so a bare comparison like
    // "< 5ms is fine" stays prose.
    if let Some(rest) = trimmed.strip_prefix('<')
        && let Some(tag) = rest.split('>').next()
        && !tag.is_empty()
        && tag.len() <= 64
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/')
        && rest.contains('>')
    {
        return true;
    }

    // A decorated shell prompt echoed into the paste.
    if trimmed.starts_with(PROMPT_DECORATION) {
        return true;
    }

    // A bare identifier rather than a sentence: no whitespace anywhere, and
    // carrying identifier punctuation. Real prompts essentially always
    // contain a space; requiring the punctuation too keeps a one-word prompt
    // like "help" or "continue" as a legitimate title.
    if !trimmed.contains(char::is_whitespace)
        && trimmed
            .chars()
            .any(|c| c.is_ascii_digit() || c == '[' || c == ']')
        && trimmed.chars().any(|c| c == '-' || c == '_' || c == '[')
    {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_observed_classes_are_scaffolding() {
        // From the #484 corpus survey: 16 IDE blocks, 4 prompt echoes, 1
        // model id across 330 real session pages.
        assert!(looks_like_scaffolding(
            "<ide_opened_file>The user opened the file /home/samir/x/main.rs"
        ));
        assert!(looks_like_scaffolding(
            "┌─[samir@samirb3 12:56:36] ~/x/ai-usagebar/gnome-extension"
        ));
        assert!(looks_like_scaffolding("claude-opus-5[1m]"));
    }

    #[test]
    fn the_adjacent_paths_known_offenders_are_covered() {
        // `transcript.rs` drops these on the workstream path; the same
        // shapes must not survive as a title here.
        assert!(looks_like_scaffolding("<system-reminder>Do not mention…"));
        assert!(looks_like_scaffolding("<user_info>cwd=/home/x"));
    }

    #[test]
    fn real_prompts_are_kept() {
        for prompt in [
            "Make the backpressure test deterministic",
            "why is the sweep deleting pinned pages?",
            "help",
            "continue",
            "< 5ms is fine for this path",
            "run bin/ci",
            "fix issue-484",
        ] {
            assert!(
                !looks_like_scaffolding(prompt),
                "{prompt:?} is user prose and must be kept"
            );
        }
    }

    #[test]
    fn a_hyphenated_word_alone_is_not_an_identifier() {
        // No digits and no brackets, so it stays prose even without a space.
        assert!(!looks_like_scaffolding("well-done"));
    }

    #[test]
    fn empty_or_blank_is_scaffolding() {
        assert!(looks_like_scaffolding(""));
        assert!(looks_like_scaffolding("   "));
    }
}
