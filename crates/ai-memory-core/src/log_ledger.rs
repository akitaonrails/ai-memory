//! Recognizing the raw hook event ledger (`log.md` / `log-YYYY-MM.md`).
//!
//! A reserved-looking filename is only a ledger when its content opens with a
//! hook log entry, never on filename alone (an ordinary page can be named
//! `log-2026-09.md`). Every surface that has to tell the two apart must agree,
//! or a hook append supersedes a real page's row — or, in the other
//! direction, a cleanup aimed at ledgers reaches a real page's version chain.
//!
//! The predicates live here, in the pure-compute layer, because the surfaces
//! that consume them do not all have the same reach:
//!
//! - the file watcher, the OKF conformance migration and the OKF bundle export
//!   hold a path on disk ([`ai_memory_wiki`](https://docs.rs/ai-memory-wiki))
//!   and open it;
//! - the store's `reclaim-ledger-versions` holds a `pages.body` it already has
//!   in memory and must not re-read from the filesystem.
//!
//! One definition, two entry points, so "is this a ledger" cannot drift
//! between the surface that indexes ledgers and the surface that deletes
//! them.

/// Upper bound on frontmatter lines scanned by [`body_opens_with_log_ledger`].
/// Conformant OKF frontmatter is a handful of keys; this is slack, not a
/// format limit.
pub const MAX_FRONTMATTER_LINES: usize = 64;

/// How much of a body a caller has to read to answer
/// [`body_opens_with_log_ledger`].
///
/// The gate is decided by the first meaningful line, so a few kilobytes is
/// already generous: it leaves room for [`MAX_FRONTMATTER_LINES`] lines of
/// frontmatter many times over. The bound is what makes the gate affordable
/// on the rows it has to classify — a hook ledger carries a whole event log,
/// and a store can hold tens of gigabytes of them. A caller holding the body
/// already (the store) and a caller reading it off disk (the indexer) both
/// need it, so the bound lives with the predicate.
pub const GATE_PREFIX_BYTES: usize = 8 * 1024;

/// Opening line of a hook log entry. The ledger is `## [ts] ...` entries and
/// never YAML frontmatter.
const LEDGER_ENTRY_PREFIX: &str = "## [";

/// True when `s` is the ledger filename shape: `log.md`, or a rotated
/// `log-YYYY-MM.md`.
///
/// This is the *shape* test only. It is deliberately satisfiable by an
/// ordinary page — pair it with [`body_opens_with_log_ledger`] before acting
/// on the answer.
#[must_use]
pub fn is_log_ledger_path(s: &str) -> bool {
    s == "log.md" || is_rotated_log_ledger_path(s)
}

/// True when `s` is the rotated ledger shape `log-YYYY-MM.md`.
///
/// Month and day are not range-checked, only digit-checked: a filename is a
/// shape, and the content gate is what decides whether a file wearing it is
/// really a ledger.
#[must_use]
pub fn is_rotated_log_ledger_path(s: &str) -> bool {
    let Some(stem) = s.strip_prefix("log-").and_then(|v| v.strip_suffix(".md")) else {
        return false;
    };
    let bytes = stem.as_bytes();
    bytes.len() == "YYYY-MM".len()
        && bytes[4] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..].iter().all(u8::is_ascii_digit)
}

/// Cheap content check for the raw hook event ledger shape. The body's first
/// meaningful line must be a hook log entry (`## [ts] ...`).
///
/// The body may sit under a YAML frontmatter block: the OKF v0.2 migration
/// conforms every `.md` under `wiki/`, ledgers included, so a migrated store
/// has `type: Note` stamped on top of each `log-YYYY-MM.md`. Stopping at the
/// fence would classify those ledgers as ordinary pages, and each hook
/// `append_event` would then supersede a multi-megabyte page row.
///
/// A body with an unterminated frontmatter block inside
/// [`MAX_FRONTMATTER_LINES`] is **not** a ledger. So is a body that ends
/// before the deciding line. Both answers say "no", which is the safe
/// direction for a caller deciding what to delete.
#[must_use]
pub fn body_opens_with_log_ledger(body: &str) -> bool {
    let mut lines = body.lines();

    let Some(first) = lines.next() else {
        return false;
    };
    if first.trim_end() != "---" {
        return first.starts_with(LEDGER_ENTRY_PREFIX);
    }

    // Walk past the frontmatter block.
    let mut closed = false;
    for line in lines.by_ref().take(MAX_FRONTMATTER_LINES) {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
    }
    if !closed {
        return false;
    }

    // First non-blank line after the fence decides.
    lines
        .find(|line| !line.trim().is_empty())
        .is_some_and(|line| line.starts_with(LEDGER_ENTRY_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_md_and_rotated_shapes_are_recognized() {
        assert!(is_log_ledger_path("log.md"));
        assert!(is_log_ledger_path("log-2026-09.md"));
        assert!(is_rotated_log_ledger_path("log-2026-09.md"));
    }

    #[test]
    fn near_miss_filenames_are_not_the_ledger_shape() {
        for s in [
            "log.md.bak",
            "log-2026-9.md",
            "log-2026-09-01.md",
            "log-2026.md",
            "log-abcdefgh.md",
            "logs/log-2026-09.md",
            "notes/log.md",
            "LOG-2026-09.md",
        ] {
            assert!(!is_log_ledger_path(s), "{s} should not be the ledger shape");
        }
    }

    #[test]
    fn a_bare_entry_opens_a_ledger() {
        assert!(body_opens_with_log_ledger(
            "## [2026-09-25T10:00:00Z] tool call"
        ));
    }

    #[test]
    fn frontmatter_does_not_hide_the_ledger() {
        let body =
            "---\ntype: Note\ntitle: Log 2026-09\n---\n\n## [2026-09-25T10:00:00Z] tool call\n";
        assert!(body_opens_with_log_ledger(body));
    }

    #[test]
    fn frontmatter_needs_a_closing_fence() {
        let body = "---\ntype: Note\n\n## [2026-09-25T10:00:00Z] tool call\n";
        assert!(!body_opens_with_log_ledger(body));
    }

    #[test]
    fn an_unterminated_frontmatter_block_is_not_a_ledger() {
        let mut body = String::from("---\n");
        for _ in 0..MAX_FRONTMATTER_LINES + 5 {
            body.push_str("type: Note\n");
        }
        body.push_str("## [2026-09-25T10:00:00Z] tool call\n");
        assert!(!body_opens_with_log_ledger(&body));
    }

    #[test]
    fn an_ordinary_page_wearing_the_ledger_name_is_not_a_ledger() {
        assert!(!body_opens_with_log_ledger(
            "# Log 2026-09\n\nA hand-written page.\n"
        ));
        assert!(!body_opens_with_log_ledger(""));
        assert!(!body_opens_with_log_ledger("---"));
    }

    #[test]
    fn a_blank_first_line_decides_against_the_ledger() {
        // The shape test is exact: the *first* line is what is compared, and
        // a leading blank line is not a fence and not an entry.
        assert!(!body_opens_with_log_ledger(
            "\n## [2026-09-25T10:00:00Z] tool call\n"
        ));
    }
}
