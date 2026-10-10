//! Destination-path safety for export and sync.
//!
//! Page paths arrive from the server as untrusted data. Every relative wiki
//! path is validated into a portable shape before it is ever joined onto the
//! destination directory, so a hostile server cannot escape `--dest`, clobber
//! dotfiles, or plant names that behave differently across filesystems.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Longest wiki path accepted, in bytes. Stays well under the shortest
/// common filesystem limit once joined onto a repository path.
pub const MAX_PATH_BYTES: usize = 180;
/// Longest single path segment accepted, in bytes.
pub const MAX_SEGMENT_BYTES: usize = 64;
/// Deepest wiki path accepted, in segments.
pub const MAX_DEPTH: usize = 8;

/// Windows reserved device stems: `CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`,
/// `LPT1`-`LPT9` (the stem is the part before the first dot, so `CON.md`
/// and `con.x.md` are both refused).
const RESERVED_STEMS: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn is_reserved_stem(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or_default();
    RESERVED_STEMS.contains(&stem.to_ascii_uppercase().as_str())
}

/// One portable path segment: ASCII letters, digits, dot, dash, underscore.
/// No leading dot (this also refuses `.`, `..` and `.git`), no trailing dot
/// (Windows quirk), no reserved device stem.
fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_SEGMENT_BYTES
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        && !segment.starts_with('.')
        && !segment.ends_with('.')
        && !is_reserved_stem(segment)
}

/// Validate one `--include` family: a single top-level wiki directory name.
/// The allowlist is explicit by design, so a bare `*` (or any wildcard) is
/// refused with its own message instead of a generic charset complaint.
pub fn validate_family(raw: &str) -> Result<()> {
    if raw == "*" {
        bail!("--include '*' is refused: the family allowlist must be explicit");
    }
    if raw.contains(['*', '?', '[', ']']) {
        bail!("--include {raw}: wildcards are not allowed; name concrete families");
    }
    if !valid_segment(raw) {
        bail!(
            "--include {raw}: not a portable directory name (ASCII letters, digits, '.', '-', '_'; \
             no leading dot, no reserved Windows name)"
        );
    }
    Ok(())
}

/// The family of a wiki path is its first segment. A bare top-level page
/// (`README.md`) is its own family and only matches an identically named
/// `--include` value.
pub fn family_of(path: &str) -> &str {
    path.split('/').next().unwrap_or(path)
}

/// Validate a server-reported wiki path before anything is joined onto
/// `--dest`. Refuses traversal, absolute paths, backslashes, dotfiles,
/// non-portable characters, oversized names and non-`.md` leaves.
pub fn validate_page_path(raw: &str) -> Result<()> {
    if raw.is_empty() {
        bail!("empty page path");
    }
    if raw.len() > MAX_PATH_BYTES {
        bail!("page path longer than {MAX_PATH_BYTES} bytes: {raw}");
    }
    if raw.starts_with('/') {
        bail!("absolute page path refused: {raw}");
    }
    if raw.contains('\\') || raw.contains('\0') || raw.bytes().any(|b| !b.is_ascii()) {
        bail!("non-portable page path refused: {raw:?}");
    }
    let segments: Vec<&str> = raw.split('/').collect();
    if segments.len() > MAX_DEPTH {
        bail!("page path deeper than {MAX_DEPTH} segments: {raw}");
    }
    for segment in &segments {
        if !valid_segment(segment) {
            bail!("non-portable path segment refused in {raw}: {segment:?}");
        }
    }
    let last = segments[segments.len() - 1];
    if !last.ends_with(".md") {
        bail!("page path does not end in .md: {raw}");
    }
    Ok(())
}

/// Refuse a set of pages whose destination paths collide once case-folded.
/// Linux is case-sensitive but macOS and Windows checkouts are not, and a
/// collision would silently clobber depending on the platform.
pub fn ensure_no_case_fold_collisions(paths: &[String]) -> Result<()> {
    let mut seen: std::collections::BTreeMap<String, &str> = std::collections::BTreeMap::new();
    for path in paths {
        let folded = path.to_ascii_lowercase();
        if let Some(first) = seen.get(folded.as_str())
            && *first != path.as_str()
        {
            bail!(
                "case-fold collision between server pages {first:?} and {path:?}; \
                 refusing to export both into one destination"
            );
        }
        seen.entry(folded).or_insert(path.as_str());
    }
    Ok(())
}

/// Join a validated relative wiki path onto the destination root, refusing
/// the join if any existing component on the way is a symlink. Components
/// that do not exist yet are fine: the writer creates them as real
/// directories. An existing non-final component that is not a directory is
/// refused so a file named like a family cannot swallow writes.
pub fn secure_join(dest_root: &Path, rel: &str) -> Result<PathBuf> {
    validate_page_path(rel)?;
    let segments: Vec<&str> = rel.split('/').collect();
    let mut current = dest_root.to_path_buf();
    for (index, segment) in segments.iter().enumerate() {
        current.push(segment);
        match fs::symlink_metadata(&current) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    bail!("refusing path through symlink: {}", current.display());
                }
                let is_last = index + 1 == segments.len();
                if !is_last && !meta.is_dir() {
                    bail!("path component is not a directory: {}", current.display());
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::Error::new(e))
                    .context(format!("cannot inspect {}", current.display()));
            }
        }
    }
    Ok(current)
}

/// Absolute path with `.` and `..` resolved lexically, without following
/// symlinks. Comparing this against `canonicalize` detects any symlink on
/// the way to the destination.
pub fn lexical_absolute(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => return path.to_path_buf(),
        }
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Prepare the destination directory: create it when missing, then refuse
/// symlinked destinations (including any symlinked component on the way —
/// on Windows this also catches junction/reparse targets via the
/// canonical-versus-lexical comparison) and non-directories. Returns the
/// canonical root every later join is anchored to.
pub fn prepare_dest(dest: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(dest) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!("--dest is a symlink; pass the real directory instead");
            }
            if !meta.is_dir() {
                bail!("--dest exists and is not a directory");
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(dest)
                .with_context(|| format!("cannot create --dest {}", dest.display()))?;
        }
        Err(e) => {
            return Err(anyhow::Error::new(e))
                .context(format!("cannot inspect --dest {}", dest.display()));
        }
    }
    let lexical = lexical_absolute(dest);
    let canonical = fs::canonicalize(dest)
        .with_context(|| format!("cannot canonicalize --dest {}", dest.display()))?;
    if canonical != lexical {
        bail!(
            "--dest reaches through a symlink ({} -> {}); pass the resolved path instead",
            lexical.display(),
            canonical.display()
        );
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_portable_page_paths() {
        for path in [
            "decisions/0007-db.md",
            "notes/deep/nested/path/is/fine.md",
            "_rules/keep-dashes_and.dots.md",
            "README.md",
            "conduct.md",
        ] {
            validate_page_path(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        }
        assert_eq!(family_of("decisions/0007-db.md"), "decisions");
        assert_eq!(family_of("README.md"), "README.md");
    }

    #[test]
    fn refuses_traversal_and_absolute_paths() {
        for path in [
            "../escape.md",
            "notes/../../escape.md",
            "/etc/passwd.md",
            "notes//double.md",
            "notes/./here.md",
            "notes/.hidden.md",
            "notes/.git/config.md",
            "",
        ] {
            assert!(validate_page_path(path).is_err(), "should refuse {path:?}");
        }
    }

    #[test]
    fn refuses_non_portable_segments() {
        for path in [
            "notes/with space.md",
            "notes/with*glob.md",
            "notes/trailing.md.",
            "notes/COM1.md",
            "notes/com2.lower.md",
            "notes/lpt9.md",
            "notes/beyond\u{e9}.md",
            "notes/readme.markdown",
            "notes/not-md.txt",
            "very/deep/path/that/goes/on/and/on/forever/here.md",
        ] {
            assert!(validate_page_path(path).is_err(), "should refuse {path:?}");
        }
        let long = format!("notes/{}.md", "a".repeat(MAX_SEGMENT_BYTES));
        assert!(validate_page_path(&long).is_err());
        let deep = format!("{}.md", ["d"; MAX_DEPTH + 1].join("/"));
        assert!(validate_page_path(&deep).is_err());
    }

    #[test]
    fn family_allowlist_is_explicit() {
        assert!(validate_family("decisions").is_ok());
        assert!(validate_family("_rules").is_ok());
        // The explicit allowlist rule: bare '*' and wildcards are refused
        // with their own message, not a generic charset error.
        let star = validate_family("*").unwrap_err().to_string();
        assert!(star.contains("must be explicit"), "{star}");
        assert!(validate_family("notes*").is_err());
        assert!(validate_family("..").is_err());
        assert!(validate_family("a/b").is_err());
        assert!(validate_family("").is_err());
    }

    #[test]
    fn detects_case_fold_collisions() {
        assert!(
            ensure_no_case_fold_collisions(&[
                "notes/a.md".to_string(),
                "decisions/b.md".to_string(),
            ])
            .is_ok()
        );
        let err =
            ensure_no_case_fold_collisions(&["Notes/a.md".to_string(), "notes/A.md".to_string()])
                .unwrap_err()
                .to_string();
        assert!(err.contains("case-fold collision"), "{err}");
    }

    #[test]
    fn prepare_dest_refuses_symlinks_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().canonicalize().unwrap();

        assert_eq!(prepare_dest(&dest).unwrap(), dest);

        let child = dest.join("real");
        fs::create_dir_all(&child).unwrap();
        assert_eq!(prepare_dest(&child).unwrap(), child);

        #[cfg(unix)]
        {
            let link = dest.join("link");
            std::os::unix::fs::symlink(&child, &link).unwrap();
            let err = prepare_dest(&link).unwrap_err().to_string();
            assert!(err.contains("symlink"), "{err}");
            // A symlinked *component* on the way is refused too.
            let through = link.join("sub");
            let err = prepare_dest(&through).unwrap_err().to_string();
            assert!(err.contains("symlink"), "{err}");
        }

        let file = dest.join("file");
        fs::write(&file, b"x").unwrap();
        let err = prepare_dest(&file).unwrap_err().to_string();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn prepare_dest_creates_missing_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().canonicalize().unwrap().join("a/b/c");
        let made = prepare_dest(&dest).unwrap();
        assert!(made.is_dir());
    }

    #[test]
    fn secure_join_stays_under_root_and_refuses_symlink_components() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let joined = secure_join(&root, "notes/page.md").unwrap();
        assert_eq!(joined, root.join("notes/page.md"));
        assert!(secure_join(&root, "../escape.md").is_err());

        #[cfg(unix)]
        {
            fs::create_dir_all(root.join("notes")).unwrap();
            std::os::unix::fs::symlink("/tmp", root.join("notes/elsewhere")).unwrap();
            let err = secure_join(&root, "notes/elsewhere/page.md")
                .unwrap_err()
                .to_string();
            assert!(err.contains("symlink"), "{err}");
        }

        // A file squatting on a family directory name is refused.
        fs::write(root.join("blob.md"), b"x").unwrap();
        assert!(secure_join(&root, "blob.md/child.md").is_err());
    }
}
