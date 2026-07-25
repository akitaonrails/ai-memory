//! Canonical CLAUDE.md / AGENTS.md routing snippet.
//!
//! This module owns the slim always-loaded base guidance that points agents at
//! the ai-memory MCP server and, when installed, the detailed ai-memory Agent
//! Skills.
//!
//! Two callers consume it:
//!
//! - `ai-memory-cli`'s `install-instructions` subcommand — writes the
//!   block into `./CLAUDE.md` directly from the host.
//! - `ai-memory-mcp`'s `memory_install_self_routing` MCP tool — returns
//!   the block plus managed skill files to the agent, which then uses its
//!   own Write/Edit tool to update the target file and skill root (the MCP
//!   server can't reach the agent's host filesystem).
//!
//! Keeping the snippet in one constant means "what gets written" stays
//! consistent across both paths; updating it once propagates.

/// HTML-comment marker that opens the managed section. Anything that
/// edits a CLAUDE.md must key off this exact string — install /
/// uninstall / refresh all locate the block by these markers.
pub const MARKER_START: &str = "<!-- ai-memory:start -->";

/// HTML-comment marker that closes the managed section.
pub const MARKER_END: &str = "<!-- ai-memory:end -->";

/// The canonical snippet body. Trimmed of leading/trailing whitespace
/// by callers; wrap with `MARKER_START` + `MARKER_END` before writing.
pub const SNIPPET_BODY: &str = r#"
## Long-term memory (ai-memory)

This project uses [ai-memory](https://github.com/akitaonrails/ai-memory)
for cross-session continuity.

**Default to the current project - always.** Every ai-memory tool
auto-scopes to the project resolved from your session's working
directory. **Do NOT pass `project`, `workspace`, or `cwd` arguments unless
the user explicitly references a *different* project by name** (e.g. "what
did we decide in the `other-app` project?"). Phrases like "this project",
"here", "we", "our work", and "where did we leave off" all mean the
*current* project, so call tools with no scoping args.

This default assumes the MCP client can identify the current agent
session. Static MCP clients in parallel sessions for the same user cannot
forward the real agent session id automatically; pass explicit
`workspace` + `project` / `scopes`, or use a session-aware bridge that
forwards the lifecycle-hook session id on MCP calls.

**Lifecycle hooks already capture sanitized, bounded prompt and tool-lifecycle
observations automatically.** They are not complete native transcripts;
managed `ai-memory run` launches add the portable visible-event ledger. Do not
manually write routine notes. Only write durable memory when the user explicitly asks
to remember or annotate something permanently.

### Use the installed ai-memory Agent Skills

Detailed tool-routing guidance lives in the installed ai-memory Agent
Skills. When a task matches an installed ai-memory Agent Skill, load and
follow that skill before calling ai-memory tools. The skills cover memory
retrieval, handoffs, durable pages, learning maintenance, and routing
install or refresh work.

### When you write a project rule, write it here

If you're about to write a durable project rule ("always X", "never
Y", "all PRs must ..."), write it in the project's canonical agent instruction file.
Many projects use CLAUDE.md for Claude Code and
AGENTS.md for Codex / OpenCode / Cursor / Gemini CLI / Grok Build CLI / Kimi Code,
but if the project says one file is canonical, use that file.

If the rule is a standing *user/team* preference that should apply to
every project (tech choices, code style, personal conventions), save it
to ai-memory's reserved global scope instead — the durable-pages skill
covers how. Default memory reads surface global-scope pages in every
project automatically.

### Refreshing this snippet

This block is maintained by ai-memory. Two ways to refresh it with the
latest binary's recommended copy:

- **From the agent** (no terminal needed): ask "refresh the ai-memory
  routing in this project". The agent calls `memory_install_self_routing`,
  picks the right filename for itself (Claude Code -> `CLAUDE.md`; Codex /
  OpenCode / Cursor / Gemini / Grok -> `AGENTS.md`; Kimi Code -> `AGENTS.md`),
  uses its Write / Edit tool to replace or append the returned
  `markered_block` while preserving
  non-ai-memory user content, then writes or updates each returned
  `managed_skills` item under the selected skill root from `target_hints`
  using its `relative_path`.
- **From the CLI**: `ai-memory install-instructions` (defaults to
  `CLAUDE.md`; pass `--target AGENTS.md` for non-Claude agents or projects
  that use `AGENTS.md` as the canonical instruction file).

Both are idempotent: re-runs replace the block delimited by the ai-memory
start/end HTML-comment markers, without disturbing the rest of the file.
"#;

/// Build the full markered block that should land in CLAUDE.md /
/// AGENTS.md, including the `<!-- ai-memory:start -->` / `<!-- ai-
/// memory:end -->` wrappers and a trailing newline.
///
/// Both the CLI's `install-instructions` and the MCP tool
/// `memory_install_self_routing` emit this exact string.
#[must_use]
pub fn full_block() -> String {
    format!("{MARKER_START}\n{}\n{MARKER_END}\n", SNIPPET_BODY.trim())
}

/// Byte offset, at or after `from`, of an occurrence of `marker` that sits
/// alone on its own line (only whitespace before it on the line and after
/// it up to the newline). [`full_block`] always writes the real delimiters
/// on their own lines, so this matches the true markers while skipping any
/// inline mention of the marker strings — a marker quoted inside prose or
/// code — which a naive [`str::find`] would hit first, truncating the
/// managed block and leaving an orphan tail on every refresh. Returns
/// `None` when no line-anchored occurrence exists.
#[must_use]
pub fn find_marker_line(haystack: &str, marker: &str, from: usize) -> Option<usize> {
    if marker.is_empty() || from > haystack.len() || !haystack.is_char_boundary(from) {
        return None;
    }

    let mut idx = from;
    while let Some(rel) = haystack[idx..].find(marker) {
        let pos = idx + rel;
        let line_start = haystack[..pos].rfind('\n').map_or(0, |n| n + 1);
        let before = &haystack[line_start..pos];
        let after = haystack[pos + marker.len()..]
            .split('\n')
            .next()
            .unwrap_or("");
        if before.trim().is_empty() && after.trim().is_empty() {
            return Some(pos);
        }
        idx = pos + marker.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_marker_line_skips_inline_mention() {
        let text = format!("a\nsee `{MARKER_END}` inline\n{MARKER_END}\nb\n");
        let pos = find_marker_line(&text, MARKER_END, 0).unwrap();
        let line_start = text[..pos].rfind('\n').map_or(0, |n| n + 1);
        assert_eq!(
            &text[line_start..pos],
            "",
            "matched marker must start its own line"
        );
        assert!(
            text[pos + MARKER_END.len()..].starts_with('\n'),
            "nothing may follow the marker on its line"
        );
    }

    #[test]
    fn find_marker_line_absent_returns_none() {
        assert!(find_marker_line("no markers here\n", MARKER_END, 0).is_none());
    }

    #[test]
    fn find_marker_line_rejects_invalid_search_inputs() {
        assert!(find_marker_line(MARKER_END, "", 0).is_none());
        assert!(find_marker_line(MARKER_END, MARKER_END, MARKER_END.len() + 1).is_none());
        assert!(find_marker_line("é\n", MARKER_END, 1).is_none());
    }

    /// Option-2 guard: the canonical block must not embed the markers
    /// anywhere but as the real delimiters, so the agent-driven install
    /// path (which never runs the CLI matcher) stays safe too.
    #[test]
    fn full_block_has_exactly_one_of_each_marker() {
        let block = full_block();
        assert_eq!(block.matches(MARKER_START).count(), 1);
        assert_eq!(block.matches(MARKER_END).count(), 1);
        assert!(block.trim_end().ends_with(MARKER_END));
    }

    /// The committed root `AGENTS.md` carries this managed block between the
    /// ai-memory markers. It is generated out-of-band
    /// (`ai-memory install-instructions --target AGENTS.md`) and committed
    /// separately, so nothing forces it to track [`SNIPPET_BODY`]. This guard
    /// fails when the two drift — regenerate to fix it. Only `AGENTS.md` is
    /// checked (root `CLAUDE.md` is a pointer; `README.md` is prose).
    #[test]
    fn committed_agents_md_matches_snippet_body() {
        // From `crates/ai-memory-core` up to the repo root.
        let agents_md = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../AGENTS.md"));
        let normalize = |s: &str| s.replace("\r\n", "\n");
        let agents_md = normalize(agents_md);

        let start = find_marker_line(&agents_md, MARKER_START, 0)
            .expect("committed AGENTS.md must contain the ai-memory start marker");
        let end = find_marker_line(&agents_md, MARKER_END, start)
            .expect("committed AGENTS.md must contain the ai-memory end marker");
        let region = &agents_md[start..end + MARKER_END.len()];

        assert_eq!(
            region,
            full_block().trim_end(),
            "committed AGENTS.md managed block is stale vs SNIPPET_BODY — \
             regenerate with `ai-memory install-instructions --target AGENTS.md --no-skills`"
        );
    }
}
