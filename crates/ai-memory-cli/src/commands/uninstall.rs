//! `ai-memory uninstall` — the symmetric inverse of install-hooks /
//! install-mcp / install-instructions. Detects ai-memory's wiring in
//! every supported agent's config and removes only that, never
//! third-party entries. Optional `--purge-data` wipes wiki/db/raw via
//! the reset path. Docker teardown is printed, never executed.
//!
//! Design: docs/superpowers/specs/2026-05-24-uninstall-command-design.md

use ai_memory_core::{MARKER_END, MARKER_START};

/// Remove the `<!-- ai-memory:start -->`…`<!-- ai-memory:end -->`
/// block (inclusive) from a CLAUDE.md / AGENTS.md. Returns the new
/// content and whether a block was found. Inverse of
/// `install_instructions::merge_instructions_block`: an install
/// followed by an uninstall round-trips to the original file.
// used by the orchestrator in a later task
#[allow(dead_code)]
fn strip_instructions_block(content: &str) -> (String, bool) {
    let Some(start) = content.find(MARKER_START) else {
        return (content.to_string(), false);
    };
    let Some(end_rel) = content[start..].find(MARKER_END) else {
        return (content.to_string(), false);
    };
    let end = start + end_rel + MARKER_END.len();
    // Consume a trailing newline after the end marker if present.
    let after = if content.as_bytes().get(end).copied() == Some(b'\n') {
        end + 1
    } else {
        end
    };
    let mut head = content[..start].to_string();
    let tail = &content[after..];
    // When the block sat at EOF, install added a blank-line separator
    // before it; drop that artifact so install→uninstall round-trips.
    if tail.is_empty() && head.ends_with("\n\n") {
        head.pop();
    }
    (format!("{head}{tail}"), true)
}

/// True when a hook command string was written by ai-memory. Install
/// inlines `AI_MEMORY_HOOK_URL=<url> [AI_MEMORY_AUTH_TOKEN=…] <path>`
/// into the command (render_shared.rs); the `AI_MEMORY_HOOK_URL=`
/// prefix is unconditional, so it is the reliable signature —
/// independent of auth, --server-url, --hooks-dir, --host-prefix.
// used by the hook stripper / orchestrator in later tasks
#[allow(dead_code)]
fn hook_command_is_ours(command: &str) -> bool {
    command.contains("AI_MEMORY_HOOK_URL=")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_instructions_round_trips_with_install_append() {
        let original = "# Title\n";
        // Mirror install_instructions::merge append behavior:
        let block = format!("{MARKER_START}\nBODY\n{MARKER_END}\n");
        let installed = format!("{original}\n{block}");
        let (stripped, found) = strip_instructions_block(&installed);
        assert!(found);
        assert_eq!(stripped, original, "uninstall must restore the original file");
    }

    #[test]
    fn strip_instructions_preserves_surrounding_content() {
        let content = format!("# Top\n\n{MARKER_START}\nBODY\n{MARKER_END}\n\nMore notes.\n");
        let (stripped, found) = strip_instructions_block(&content);
        assert!(found);
        assert!(stripped.contains("# Top"));
        assert!(stripped.contains("More notes."));
        assert!(!stripped.contains("BODY"));
        assert!(!stripped.contains(MARKER_START));
    }

    #[test]
    fn strip_instructions_no_block_is_noop() {
        let content = "# Just a readme\n";
        let (stripped, found) = strip_instructions_block(content);
        assert!(!found);
        assert_eq!(stripped, content);
    }

    #[test]
    fn hook_signature_matches_no_auth_default() {
        let cmd = "AI_MEMORY_HOOK_URL=http://127.0.0.1:49374 /home/u/.local/share/ai-memory/hooks/claude-code/stop.sh";
        assert!(hook_command_is_ours(cmd));
    }

    #[test]
    fn hook_signature_matches_with_auth_and_custom_prefix() {
        let cmd = "AI_MEMORY_HOOK_URL=http://lan:49374 AI_MEMORY_AUTH_TOKEN=abc /etc/custom/session-start.sh";
        assert!(hook_command_is_ours(cmd));
    }

    #[test]
    fn hook_signature_rejects_third_party_with_generic_name() {
        // A user's own hook that happens to be named stop.sh — no prefix.
        assert!(!hook_command_is_ours("/usr/local/bin/my-stop.sh"));
        assert!(!hook_command_is_ours("/opt/tools/hooks/session-start.sh"));
    }
}
