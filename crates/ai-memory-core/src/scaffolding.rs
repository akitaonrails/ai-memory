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
//!
//! **A bare model id is not tested for here, on purpose.** It has the same
//! shape as a terse human reference — `gpt-5` and `pr-477` differ only in
//! meaning — so any lexical rule that catches one catches the other, and this
//! predicate errs toward keeping text. Every model id that reached a title
//! in the surveyed corpus arrived on a `SessionStart` observation, which is
//! the only kind whose title `best_title_hint` fills from the harness's
//! `model` field, so `derive_title` skips that kind outright instead.
//!
//! That is narrower than a shape rule, deliberately: a model id somebody
//! *typed* is now kept, because at that point it is what the user wrote.

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

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_shape_classes_are_scaffolding() {
        // From the #484 corpus survey: 16 IDE blocks and 4 prompt echoes
        // across 330 real session pages. The survey's third class, a bare
        // model id, is excluded at its source instead — see below.
        assert!(looks_like_scaffolding(
            "<ide_opened_file>The user opened the file /home/samir/x/main.rs"
        ));
        assert!(looks_like_scaffolding(
            "┌─[samir@samirb3 12:56:36] ~/x/ai-usagebar/gnome-extension"
        ));
    }

    #[test]
    fn a_bare_identifier_is_not_judged_by_shape() {
        // A model id and a terse human reference are the same string shape,
        // so a rule catching `claude-opus-5[1m]` also catches every entry
        // below. Measured against the corpus survey's two populations, one
        // such rule matched 10 of 10 model ids and 7 of 11 references.
        // Keeping text is the cheaper error: `derive_title` excludes model
        // ids by observation kind, and nothing excludes a lost title.
        for reference in [
            "pr-477",
            "issue-484",
            "fix-484",
            "commit-a0bc43d",
            "main-2.rs",
            "step-1",
            "task_7",
        ] {
            assert!(
                !looks_like_scaffolding(reference),
                "{reference:?} is a plausible terse prompt and must survive"
            );
        }
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
