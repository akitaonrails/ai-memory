//! The "reload context" request: a prompt asking for a recap of the session
//! stretch the last compaction dropped. Pure; the hook client checks it
//! before fetching `GET /recap`, and the OpenCode plugins get the same rule
//! as a regex.

const PHRASE: &str = "reload context";

/// The first line of every post-compaction anchor `GET /recap?mode=anchor`
/// renders for a readable session, up to the session id. The hook client and
/// the OpenCode plugins inject a reply only when it starts with this, so an
/// older server's full recap or a refusal notice never lands automatically.
pub const ANCHOR_HEADER_PREFIX: &str = "> 🧭 ai-memory: post-compaction anchor for this session";

/// The anchor's reply for a session the store does not know or the caller may
/// not read. It deliberately does not start with [`ANCHOR_HEADER_PREFIX`].
pub const NO_ANCHOR_SESSION_NOTICE: &str = "> 🧭 ai-memory: post-compaction anchor: no session is \
     recorded under this id for you, so there is nothing to re-ground on.\n";

/// [`is_reload_request`] as a JavaScript regex literal, for the generated
/// OpenCode plugins.
pub const RELOAD_TRIGGER_JS_REGEX: &str = r"/^\s*reload context(?![\p{L}\p{N}_])/iu";

/// `true` when the prompt opens with `reload context` (ASCII
/// case-insensitive, after leading whitespace) as whole words. Anchored at
/// the start so a sentence that only mentions the phrase does not fire.
#[must_use]
pub fn is_reload_request(prompt: &str) -> bool {
    let rest = prompt.trim_start();
    let Some(head) = rest.get(..PHRASE.len()) else {
        return false;
    };
    if !head.eq_ignore_ascii_case(PHRASE) {
        return false;
    }
    rest[PHRASE.len()..]
        .chars()
        .next()
        .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unknown_session_notice_is_not_an_anchor() {
        assert!(!NO_ANCHOR_SESSION_NOTICE.starts_with(ANCHOR_HEADER_PREFIX));
    }

    #[test]
    fn reload_trigger_table() {
        for (prompt, want) in [
            ("reload context", true),
            ("reload context after last compaction, then continue", true),
            ("  Reload Context: then fix the build", true),
            ("RELOAD CONTEXT\nand go", true),
            ("reload context.", true),
            ("reload contexts", false),
            ("reload context_x", false),
            ("reload contexté", false),
            ("please reload context", false),
            ("can you reload context later?", false),
            ("reload", false),
            ("", false),
        ] {
            assert_eq!(is_reload_request(prompt), want, "{prompt:?}");
        }
    }

    #[test]
    fn js_regex_names_the_same_phrase() {
        assert_eq!(
            RELOAD_TRIGGER_JS_REGEX,
            r"/^\s*reload context(?![\p{L}\p{N}_])/iu"
        );
    }
}
