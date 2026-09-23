//! Shared model-path sanitization for LLM-produced wiki paths.
//!
//! Bootstrap (#847), per-session consolidation (#848), and auto-improve
//! review all accept a page path straight from LLM structured output.
//! Bootstrap and consolidation hand it to `Wiki::apply_batch`, which is
//! atomic: a single path that fails `PagePath::ensure_portable` at write
//! time aborts every page in that batch, not just its own. Auto-improve
//! stages the path and only hits `ensure_portable` on approve, so a bad
//! path becomes an unapplyable proposal. `PagePath::new` is deliberately
//! tolerant (see its doc comment) and does not catch this, so callers must
//! sanitize the raw model path themselves before constructing a `PagePath`.

/// Filename characters Windows refuses, mirroring
/// `ai_memory_core::ids`'s reserved-char set, plus `\` — `PagePath::new`
/// already rejects a literal backslash anywhere in the raw path (it reads as
/// a separator), so a component containing one must be cleaned before
/// `PagePath::new` ever sees it, not after.
pub(crate) const PATH_ILLEGAL_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*', '\\'];

/// Clean a model-produced page path so it survives `PagePath::new` and
/// `ensure_portable`.
///
/// The LLM sometimes echoes free text — a conventional-commit subject like
/// `build(sandbox): orchestrate` — straight into a page path. That passes
/// `PagePath::new` (deliberately tolerant; see its doc comment) but fails
/// `ensure_portable`, which `Wiki::apply_batch` enforces atomically: one bad
/// path there aborts every page in the batch, not just its own (#847, #848).
/// Auto-improve review stages the path and hits the same check on approve.
/// Replace every Windows-illegal character and ASCII control byte in each
/// `/`-separated component with `-`, keeping the `dir/subdir/name.md` shape
/// intact so the model's intended layout survives.
pub(crate) fn slugify_page_path(raw: &str) -> String {
    raw.split('/')
        .map(|segment| {
            segment
                .chars()
                .map(|c| {
                    if PATH_ILLEGAL_CHARS.contains(&c) || (c as u32) < 0x20 {
                        '-'
                    } else {
                        c
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::slugify_page_path;

    #[test]
    fn slugify_page_path_replaces_illegal_chars_and_keeps_slashes() {
        assert_eq!(
            slugify_page_path("concepts/build(sandbox): orchestrate the run.md"),
            "concepts/build(sandbox)- orchestrate the run.md"
        );
        assert_eq!(
            slugify_page_path("a/b<c>d:e\"f|g?h*i\\j.md"),
            "a/b-c-d-e-f-g-h-i-j.md"
        );
        assert_eq!(
            slugify_page_path("concepts/clean-path.md"),
            "concepts/clean-path.md",
            "an already-portable path must be left unchanged"
        );
    }
}
