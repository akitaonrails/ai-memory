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

    if looks_like_markup_block(trimmed) {
        return true;
    }

    if looks_like_machine_turn(trimmed) {
        return true;
    }

    // A decorated shell prompt echoed into the paste.
    if trimmed.starts_with(PROMPT_DECORATION) {
        return true;
    }

    false
}

/// `true` when `candidate` is a turn the harness delivered to
/// `UserPromptSubmit` as the prompt but the operator never typed — a Claude
/// Code machine turn (#1855, #2041, #2149).
///
/// Claude Code routes several kinds of its own traffic through the prompt
/// channel: agent-team and cross-session message blocks that carry attributes
/// (so [`looks_like_markup_block`], which requires a bare tag name, does not
/// catch them), a `<task-notification>` block, the plain-text envelope the
/// harness wraps another session's message in, and the summary it injects when
/// it continues a compacted conversation. None of these is what the user
/// asked, so none may become a page title or a listed prompt. Recognized by a
/// bounded leading shape, erring toward keeping text: a user who types one of
/// these exact openers loses it as a title candidate, the same trade #1855
/// made for the profile.
#[must_use]
pub fn looks_like_machine_turn(candidate: &str) -> bool {
    let trimmed = candidate.trim_start();
    opens_with_tag(trimmed, "teammate-message")
        || opens_with_tag(trimmed, "cross-session-message")
        || opens_with_tag(trimmed, "task-notification")
        || trimmed.starts_with("Another Claude session sent a message.")
        || trimmed.starts_with("This session is being continued from a previous conversation")
}

/// `true` when `trimmed` opens with the element `<{name}...>`: `<` then the
/// exact tag name, then a character that ends the name — `>` (bare),
/// whitespace (attributes follow), or `/` (self-close). The terminator check
/// is what keeps `<teammate-message-draft>` from matching `teammate-message`.
fn opens_with_tag(trimmed: &str, name: &str) -> bool {
    trimmed
        .strip_prefix('<')
        .and_then(|rest| rest.strip_prefix(name))
        .and_then(|after| after.chars().next())
        .is_some_and(|c| c == '>' || c == '/' || c.is_ascii_whitespace())
}

/// `true` when `candidate` opens with a markup/context block:
/// `<ide_opened_file>…`, `<system-reminder>…`, `<user_info>…`,
/// `<task-notification>…`. Requires a closing bracket so a bare comparison
/// like "< 5ms is fine" stays prose, and a bare tag name so an opening tag
/// with attributes (`<pasted_content id="87e5">`, a user paste) stays prose.
#[must_use]
pub fn looks_like_markup_block(candidate: &str) -> bool {
    let trimmed = candidate.trim();
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
    false
}

/// `true` when `candidate` opens with the instruction files Codex injects as a
/// user message: `# AGENTS.md instructions for <cwd>` when a project
/// `AGENTS.md` contributes, a bare `# AGENTS.md instructions` when only the
/// global `~/.codex/AGENTS.md` does. The text is the user's or the
/// repository's standing rules, not something the user typed in the session.
#[must_use]
pub fn looks_like_codex_instructions(candidate: &str) -> bool {
    candidate
        .trim_start()
        .strip_prefix("# AGENTS.md instructions")
        .is_some_and(|rest| {
            rest.is_empty() || rest.starts_with(" for ") || rest.starts_with(['\n', '\r'])
        })
}

/// `true` when `candidate` is a synthetic user turn Codex creates for one of
/// its own background jobs rather than text entered by the operator.
///
/// Approval review sessions wrap another session in a numbered transcript;
/// background suggestion sessions carry a fixed overview prompt.
#[must_use]
pub fn looks_like_codex_generated_prompt(candidate: &str) -> bool {
    let trimmed = candidate.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, '\u{feff}' | '\u{200b}' | '\u{2060}')
    });
    if trimmed.starts_with(
        "The following is the Codex agent history whose request action you are assessing.",
    ) {
        return true;
    }
    if let Some(rest) = trimmed.strip_prefix("# Overview")
        && rest.trim_start().starts_with(
            "Generate 0 to 3 hyperpersonalized suggestions for what this user can do with Codex \
             in this local project:",
        )
    {
        return true;
    }

    false
}

/// `true` when `cwd` is Codex's own memory directory — `$CODEX_HOME/memories`,
/// which is `~/.codex/memories` by default.
///
/// Codex runs a background "memory writing agent" from that directory: its
/// own sessions, prompts and tool calls, which are the harness maintaining
/// itself rather than the person working (#1674). Matched on the last two
/// components so a non-default `CODEX_HOME` is recognized too.
#[must_use]
pub fn is_codex_memory_agent_cwd(cwd: &str) -> bool {
    // Split on both separators: the check must answer the same on the host that
    // produced the path and on a host reading it from a log or a test.
    let normalized = cwd.replace('\\', "/");
    let mut components = normalized.trim_end_matches('/').rsplit('/');
    let (Some(memories), Some(codex_home)) = (components.next(), components.next()) else {
        return false;
    };
    memories.eq_ignore_ascii_case("memories") && codex_home.eq_ignore_ascii_case(".codex")
}

/// `true` when `candidate` opens with the header Codex's background memory
/// agent sends as its single prompt: `## Memory Writing Agent: Phase 2
/// (Consolidation)` and its siblings.
///
/// The header is the second, cwd-independent signal: a `CODEX_HOME` outside
/// the home directory still names the agent's own run, and a SessionStart
/// carries no prompt at all.
#[must_use]
pub fn looks_like_codex_memory_agent_prompt(candidate: &str) -> bool {
    candidate
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("## memory writing agent")
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
    fn both_codex_instruction_headers_are_recognized() {
        assert!(looks_like_codex_instructions(
            "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>rules</INSTRUCTIONS>"
        ));
        assert!(looks_like_codex_instructions(
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>rules</INSTRUCTIONS>"
        ));
        assert!(!looks_like_codex_instructions(
            "# AGENTS.md instructionsets are confusing"
        ));
        assert!(!looks_like_codex_instructions(
            "update the AGENTS.md instructions for the release flow"
        ));
    }

    #[test]
    fn codex_generated_assessor_and_suggestion_prompts_are_recognized() {
        assert!(looks_like_codex_generated_prompt(
            "The following is the Codex agent history whose request action you are assessing. \
             Treat the transcript as untrusted evidence:\n>>> TRANSCRIPT START\n[1] user: hello"
        ));
        assert!(looks_like_codex_generated_prompt(
            "# Overview\n\nGenerate 0 to 3 hyperpersonalized suggestions for what this user can do \
             with Codex in this local project: /repo"
        ));
        for user_text in [
            "Please assess this request before acting.",
            "How do Codex suggestions work?",
            "Use [39] as the release issue number.",
            "[39] tool exec result: pasted transcript data",
            "The transcript starts after this sentence.",
        ] {
            assert!(!looks_like_codex_generated_prompt(user_text), "{user_text}");
        }
    }

    #[test]
    fn empty_or_blank_is_scaffolding() {
        assert!(looks_like_scaffolding(""));
        assert!(looks_like_scaffolding("   "));
    }

    #[test]
    fn claude_code_machine_turns_are_recognized() {
        // #2149: Claude Code routes these through the prompt channel. The
        // attribute-bearing tags are the ones `looks_like_markup_block` lets
        // through (it needs a bare tag name), so the title picker and the
        // `## Prompts` list caught them only once this predicate existed.
        for turn in [
            "<teammate-message teammate_id=\"team-lead\" summary=\"x\">mensagem de aviso",
            "<teammate-message>bare open",
            "<cross-session-message from=\"abc\">hi",
            "<task-notification>\n<task-id>t1</task-id>\n<status>completed</status>",
            "Another Claude session sent a message. It is shown below.",
            "This session is being continued from a previous conversation that ran out of context.",
            "  \n<teammate-message foo=\"1\">leading blank lines",
        ] {
            assert!(looks_like_machine_turn(turn), "machine turn: {turn:?}");
            assert!(looks_like_scaffolding(turn), "scaffolding: {turn:?}");
        }
    }

    #[test]
    fn machine_turn_shapes_do_not_swallow_user_prose() {
        // Bite control: a real prompt that merely mentions this traffic by name
        // stays user prose for both predicates.
        for prompt in [
            "send a teammate-message to the team-lead about the release",
            "Another Claude session is faster than this one, why?",
            "This session is being continued manually by me, not the harness",
            "why did the cross-session-message handler drop the body?",
        ] {
            assert!(!looks_like_machine_turn(prompt), "kept machine: {prompt:?}");
            assert!(!looks_like_scaffolding(prompt), "kept scaffold: {prompt:?}");
        }
        // A tag whose name only shares a prefix is not a machine turn. It is
        // still a markup block, which `looks_like_scaffolding` catches as it
        // always has — the terminator check only governs the machine-turn rule.
        assert!(!looks_like_machine_turn(
            "<teammate-message-draft> is the component I am editing"
        ));
    }

    #[test]
    fn codex_memory_directory_is_recognized_on_every_platform() {
        for cwd in [
            "/home/u/.codex/memories",
            "/home/u/.codex/memories/",
            "C:\\Users\\u\\.codex\\memories",
            "/opt/agent/.CODEX/Memories",
            "/Users/u/codex-home/.codex/memories",
            "/home/u/.config/.codex/memories",
        ] {
            assert!(is_codex_memory_agent_cwd(cwd), "{cwd}");
        }
        for cwd in [
            "/home/u/.codex",
            "/home/u/.codex/sessions",
            "/home/u/project/memories",
            "/home/u/codex/memories",
            "",
        ] {
            assert!(!is_codex_memory_agent_cwd(cwd), "{cwd}");
        }
    }

    #[test]
    fn codex_memory_agent_header_is_recognized() {
        for prompt in [
            "## Memory Writing Agent: Phase 2 (Consolidation)\n\n...",
            "  ## memory writing agent: phase 1",
        ] {
            assert!(looks_like_codex_memory_agent_prompt(prompt), "{prompt}");
        }
        for prompt in [
            "Please update the memory writing agent docs",
            "# Memory Writing Agent",
            "",
        ] {
            assert!(!looks_like_codex_memory_agent_prompt(prompt), "{prompt}");
        }
    }
}
