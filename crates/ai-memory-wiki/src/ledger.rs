//! Shared helpers for recognizing the raw hook event ledger
//! (`log.md` / `log-YYYY-MM.md`) by filename shape and by content.
//!
//! Both the watcher's indexer (#660) and the OKF conformance migration
//! (#669) must treat these files identically: a reserved-looking filename
//! is only a ledger when its content opens with a hook log entry, never on
//! filename alone (an ordinary page can be named `log-2026-09.md`).

use std::path::Path;

use ai_memory_core::PagePath;

/// `log.md` / `log-YYYY-MM.md` are the raw per-project event ledger the hooks
/// append to (see `ai-memory-hooks::log::log_filename_for`): `## [ts] ...`
/// entries, never YAML frontmatter.
pub(crate) fn is_log_ledger_filename(page_path: &PagePath) -> bool {
    let s = page_path.as_str();
    s == "log.md" || is_rotated_log_filename(s)
}

pub(crate) fn is_rotated_log_filename(s: &str) -> bool {
    let Some(stem) = s.strip_prefix("log-").and_then(|v| v.strip_suffix(".md")) else {
        return false;
    };
    let bytes = stem.as_bytes();
    bytes.len() == "YYYY-MM".len()
        && bytes[4] == b'-'
        && bytes[..4].iter().all(|b| b.is_ascii_digit())
        && bytes[5..].iter().all(|b| b.is_ascii_digit())
}

/// Cheap check for the raw hook event ledger shape. A reserved-looking
/// filename is only a ledger when its first body line is a hook log entry
/// (`## [ts] ...`).
///
/// The body may sit under a YAML frontmatter block: the OKF v0.2 migration
/// conforms every `.md` under `wiki/`, ledgers included, so a migrated store
/// has `type: Note` stamped on top of each `log-YYYY-MM.md`. Stopping at the
/// fence would classify those ledgers as ordinary pages, and each hook
/// `append_event` would then supersede a multi-megabyte page row.
pub(crate) fn opens_with_log_ledger(abs: &Path) -> bool {
    use std::io::{BufRead, BufReader};
    let Ok(file) = std::fs::File::open(abs) else {
        return false;
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return false;
    }
    if line.trim_end() != "---" {
        return line.starts_with("## [");
    }
    // Walk past the frontmatter block. The bound keeps a pathological file
    // (a lone opening fence in a multi-gigabyte log) from a full scan.
    let mut closed = false;
    for _ in 0..MAX_FRONTMATTER_LINES {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return false,
            Ok(_) => {}
        }
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
    }
    if !closed {
        return false;
    }
    // First non-blank line after the fence decides.
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return false,
            Ok(_) => {}
        }
        if !line.trim().is_empty() {
            return line.starts_with("## [");
        }
    }
}

/// Upper bound on frontmatter lines scanned by [`opens_with_log_ledger`].
/// Conformant OKF frontmatter is a handful of keys; this is slack, not a
/// format limit.
pub(crate) const MAX_FRONTMATTER_LINES: usize = 64;
