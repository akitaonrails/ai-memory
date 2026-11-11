//! Gemini CLI's `BeforeAgent` and `AfterAgent` hooks carry the request Gemini
//! built for the model as `prompt`, not the text the user typed.
//!
//! The hook reference calls that field "the original text submitted by the
//! user" (<https://github.com/google-gemini/gemini-cli/blob/5a5c7180ff3f0a02511f54b54db7f228d4982a30/docs/hooks/reference.md#L145-L171>),
//! but the client hands the hook `partToString(request)` of the query
//! `handleAtCommand` processed
//! (<https://github.com/google-gemini/gemini-cli/blob/5a5c7180ff3f0a02511f54b54db7f228d4982a30/packages/core/src/core/client.ts#L166-L194>).
//! After the user's words that query holds a note naming any agent picked with
//! `@` and the contents of every `@`-referenced file and MCP resource, after a
//! `--- Content from referenced files ---` line
//! (<https://github.com/google-gemini/gemini-cli/blob/5a5c7180ff3f0a02511f54b54db7f228d4982a30/packages/cli/src/ui/hooks/atCommandProcessor.ts#L735-L765>).
//! Those contents are not the prompt, and a file the project's `ignore_paths`
//! keeps out of capture would reach the spool and the store inside it.
//! [`strip_gemini_prompt_expansion`] cuts the prompt where the expansion
//! starts: on the client before the native hook spools the event, and on the
//! server before the envelope reads it.

use ai_memory_core::AgentKind;

/// `REFERENCE_CONTENT_START` (packages/core/src/utils/constants.ts), which
/// `handleAtCommand` pushes on a line of its own before the referenced
/// contents.
const REFERENCED_CONTENT_HEADER: &str = "\n--- Content from referenced files ---";

/// The start of the note `handleAtCommand` adds when the user picks an agent
/// with `@`.
const SELECTED_AGENT_NOTE: &str =
    "\n<system_note>\nThe user has explicitly selected the following agent(s): ";

/// Cut a Gemini CLI hook's `prompt` where Gemini's own expansion of the user's
/// words begins, and report whether anything was removed. Any other agent, a
/// payload without a string `prompt`, and a prompt Gemini did not expand are
/// left as they are.
pub fn strip_gemini_prompt_expansion(agent: AgentKind, raw: &mut serde_json::Value) -> bool {
    if agent != AgentKind::GeminiCli {
        return false;
    }
    let Some(serde_json::Value::String(prompt)) = raw.get_mut("prompt") else {
        return false;
    };
    let Some(cut) = [SELECTED_AGENT_NOTE, REFERENCED_CONTENT_HEADER]
        .iter()
        .filter_map(|marker| prompt.find(marker))
        .min()
    else {
        return false;
    };
    prompt.truncate(cut);
    true
}
