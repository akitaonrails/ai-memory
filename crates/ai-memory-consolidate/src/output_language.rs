//! Optional operator override that fixes the natural language of generated
//! memory prose, regardless of the input's dominant language (#2185).

/// Append an output-language override to a generation system prompt.
///
/// `output_language` is `None` for the default behavior — the base prompt's
/// "dominant natural language of the input" rule stands and the returned
/// string is the base prompt unchanged. When `Some(lang)`, an authoritative
/// override section is appended that supersedes that rule: all generated prose
/// is written in `lang`, while evidence quotes, code, identifiers, paths, and
/// raw history stay verbatim. The caller passes an already-validated,
/// non-empty, single-line language name.
#[must_use]
pub fn with_output_language(base: &str, output_language: Option<&str>) -> String {
    match output_language {
        None => base.to_string(),
        Some(lang) => format!(
            "{base}\n\n## OUTPUT LANGUAGE — OPERATOR OVERRIDE\n\n\
The operator configured a fixed output language for generated memory. This \
overrides the \"dominant natural language of the input\" instruction above: \
write every generated page title, markdown prose, patch content, and \
rationale in {lang}, regardless of the language the input is written in. Keep \
evidence quotes, code, identifiers, file paths, shell commands, error strings, \
patch anchors, and raw session history verbatim in their original language — \
do not translate them. JSON keys, enum values, and paths stay exactly as the \
schema requires."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_returns_base_unchanged() {
        let base = "SYSTEM PROMPT BODY\n\n## OUTPUT LANGUAGE\n\nWrite in the dominant natural language of the input.";
        assert_eq!(with_output_language(base, None), base);
    }

    #[test]
    fn some_appends_authoritative_override_and_preserves_base() {
        let base = "BODY with dominant natural language of the input rule.";
        let out = with_output_language(base, Some("English"));
        assert!(
            out.starts_with(base),
            "base prompt must be preserved verbatim"
        );
        assert!(out.contains("## OUTPUT LANGUAGE — OPERATOR OVERRIDE"));
        assert!(out.contains("write every generated page title, markdown prose, patch content, and rationale in English"));
        assert!(out.contains("verbatim in their original language"));
        // The override comes last so it is the authoritative instruction.
        assert!(
            out.rfind("OPERATOR OVERRIDE").unwrap()
                > out.find("dominant natural language").unwrap()
        );
    }
}
