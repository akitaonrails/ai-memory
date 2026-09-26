//! Shared helpers for recognizing the raw hook event ledger
//! (`log.md` / `log-YYYY-MM.md`) by filename shape and by content.
//!
//! The watcher's indexer (#660), the OKF conformance migration (#669) and
//! the OKF bundle export (#748) must treat these files identically: a
//! reserved-looking filename is only a ledger when its content opens with a
//! hook log entry, never on filename alone (an ordinary page can be named
//! `log-2026-09.md`).
//!
//! The predicates themselves live in [`ai_memory_core::log_ledger`], because
//! the store's `reclaim-ledger-versions` has to answer the same question from
//! a `pages.body` it already holds, with no filesystem access. This module is
//! only the file-reading half of that contract.

use std::path::Path;

use ai_memory_core::PagePath;

/// `log.md` / `log-YYYY-MM.md` are the raw per-project event ledger the hooks
/// append to (see `ai-memory-hooks::log::log_filename_for`).
pub(crate) fn is_log_ledger_filename(page_path: &PagePath) -> bool {
    ai_memory_core::log_ledger::is_log_ledger_path(page_path.as_str())
}

pub(crate) fn is_rotated_log_filename(s: &str) -> bool {
    ai_memory_core::log_ledger::is_rotated_log_ledger_path(s)
}

/// Cheap check for the raw hook event ledger shape. A reserved-looking
/// filename is only a ledger when its first body line is a hook log entry
/// (`## [ts] ...`).
///
/// Reads only [`GATE_PREFIX_BYTES`] and defers the decision to
/// [`ai_memory_core::log_ledger::body_opens_with_log_ledger`], so the file and
/// the in-memory answer cannot drift apart. The bound is also what keeps a
/// pathological file (a lone opening fence in a multi-gigabyte log) from a
/// full scan.
pub(crate) fn opens_with_log_ledger(abs: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(abs) else {
        return false;
    };
    let mut prefix = vec![0u8; ai_memory_core::log_ledger::GATE_PREFIX_BYTES];
    let read = match file.read(&mut prefix) {
        Ok(n) => n,
        Err(_) => return false,
    };
    // A file that filled the buffer may have been cut mid-line; a shorter read
    // is the whole file, last line and all.
    if read == prefix.len() {
        match prefix.iter().rposition(|b| *b == b'\n') {
            Some(last) => prefix.truncate(last + 1),
            None => prefix.clear(),
        }
    } else {
        prefix.truncate(read);
    }
    match std::str::from_utf8(&prefix) {
        Ok(text) => ai_memory_core::log_ledger::body_opens_with_log_ledger(text),
        // A ledger entry is ASCII, so invalid UTF-8 this early means the file
        // is not one.
        Err(_) => false,
    }
}

/// True when `abs` is a **rotated** event ledger (`log-YYYY-MM.md`) and not an
/// ordinary page that merely carries that name: the filename shape and the
/// body's opening line have to agree, the same content gate the migration scan
/// applies (#669).
///
/// `log.md` is left to the caller: OKF reserves that name, so a consumer of
/// this crate already excludes it by reserved name, with or without ledger
/// content. Only the rotated form needs ai-memory-specific knowledge.
pub fn is_rotated_event_ledger(abs: &Path) -> bool {
    abs.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_rotated_log_filename)
        && opens_with_log_ledger(abs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write `body` to a fresh file in a throwaway dir and hand back the path.
    fn temp_file(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    /// The file reader and the in-memory predicate must answer identically:
    /// the store's `reclaim-ledger-versions` decides from a `pages.body` and
    /// the indexer decides from this file, and the two surfaces have to agree
    /// about which paths are ledgers.
    #[test]
    fn the_file_and_the_body_answer_the_same_question() {
        let dir = tempfile::tempdir().unwrap();
        let cases: &[(&str, &str)] = &[
            ("bare.md", "## [2026-09-25T10:00:00Z] tool call\n"),
            (
                "conformed.md",
                "---\ntype: Note\ntitle: Log 2026-09\n---\n\n## [2026-09-25T10:00:00Z] tool call\n",
            ),
            ("prose.md", "# Log 2026-09\n\nA hand-written page.\n"),
            ("frontmatter-only.md", "---\ntype: Note\ntitle: Log\n---\n"),
            ("unterminated.md", "---\ntype: Note\n"),
            ("empty.md", ""),
            ("blank-first.md", "\n## [2026-09-25T10:00:00Z] tool call\n"),
            (
                "no-trailing-newline.md",
                "## [2026-09-25T10:00:00Z] tool call",
            ),
        ];
        for (name, body) in cases {
            let path = temp_file(&dir, name, body);
            assert_eq!(
                opens_with_log_ledger(&path),
                ai_memory_core::log_ledger::body_opens_with_log_ledger(body),
                "file and body disagree for {name}",
            );
        }
    }

    /// A body longer than the gate prefix must still be classified from its
    /// first meaningful line, not read whole.
    #[test]
    fn a_ledger_longer_than_the_gate_prefix_is_still_a_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let filler = "## [2026-09-25T10:00:00Z] tool Bash: cargo test\n".repeat(4000);
        assert!(filler.len() > ai_memory_core::log_ledger::GATE_PREFIX_BYTES);
        let path = temp_file(&dir, "long.md", &filler);
        assert!(opens_with_log_ledger(&path));
    }

    /// A prefix cut mid-line must not answer for a line it only half read.
    #[test]
    fn a_prefix_cut_mid_line_does_not_decide() {
        let dir = tempfile::tempdir().unwrap();
        // Push the deciding line past the prefix, with a huge frontmatter
        // value so the cut lands inside it.
        let mut body = String::from("---\ntitle: ");
        body.push_str(&"x".repeat(ai_memory_core::log_ledger::GATE_PREFIX_BYTES));
        body.push_str("\n---\n\n## [2026-09-25T10:00:00Z] tool call\n");
        let path = temp_file(&dir, "cut.md", &body);
        assert!(
            !opens_with_log_ledger(&path),
            "an unread deciding line is not a ledger"
        );
    }

    /// A missing file is not a ledger, and neither reader may panic on it.
    #[test]
    fn a_missing_file_is_not_a_ledger() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!opens_with_log_ledger(&dir.path().join("absent.md")));
        assert!(!is_rotated_event_ledger(&dir.path().join("absent.md")));
    }
}
