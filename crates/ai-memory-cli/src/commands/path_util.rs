//! Path helpers shared by command renderers and hook capture.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Resolve the user home used for agent configuration paths.
///
/// Tests and scripted wrappers can set `AI_MEMORY_HOME` to exercise the
/// platform-specific config layout without touching the real user profile.
pub(crate) fn home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("AI_MEMORY_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    dirs::home_dir()
}

/// Claude Code's relocated config root: `$CLAUDE_CONFIG_DIR` when set to a
/// non-empty, non-whitespace value, else `None` (callers fall back to the
/// `~/.claude*` defaults). The env value comes in as a parameter so tests
/// can exercise both branches without mutating process env.
pub(crate) fn claude_config_dir(env_override: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let value = env_override?;
    if value.to_str().is_some_and(|s| s.trim().is_empty()) {
        return None;
    }
    Some(PathBuf::from(value))
}

/// Candidate Claude Code paths for uninstall. The active relocated path comes
/// first, followed by the legacy home path, with exact duplicates removed.
pub(crate) fn claude_config_paths(
    home: Option<&Path>,
    relocated: Option<&Path>,
    legacy_relative: &Path,
    relocated_relative: &Path,
) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(2);
    if let Some(root) = relocated {
        paths.push(root.join(relocated_relative));
    }
    if let Some(root) = home {
        let legacy = root.join(legacy_relative);
        if !paths.contains(&legacy) {
            paths.push(legacy);
        }
    }
    paths
}

/// Strip only Windows verbatim path prefixes that are safe to render as plain
/// paths. Unknown verbatim forms are left unchanged.
pub(crate) fn strip_windows_verbatim_prefix(path: &str) -> Cow<'_, str> {
    let Some(rest) = path.strip_prefix(r"\\?\") else {
        return Cow::Borrowed(path);
    };

    if rest.len() >= 3
        && rest.as_bytes()[0].is_ascii_alphabetic()
        && rest.as_bytes()[1] == b':'
        && rest.as_bytes()[2] == b'\\'
    {
        return Cow::Borrowed(rest);
    }

    let bytes = rest.as_bytes();
    if bytes.len() >= 4
        && bytes[0].eq_ignore_ascii_case(&b'U')
        && bytes[1].eq_ignore_ascii_case(&b'N')
        && bytes[2].eq_ignore_ascii_case(&b'C')
        && bytes[3] == b'\\'
    {
        return Cow::Owned(format!(r"\\{}", &rest[4..]));
    }

    Cow::Borrowed(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn claude_config_dir_honours_non_empty_value() {
        assert_eq!(
            claude_config_dir(Some(OsString::from("/stores/claude"))),
            Some(PathBuf::from("/stores/claude"))
        );
    }

    #[test]
    fn claude_config_dir_treats_unset_empty_and_whitespace_as_unset() {
        for env in [None, Some(OsString::new()), Some(OsString::from("   "))] {
            assert_eq!(claude_config_dir(env.clone()), None, "env {env:?}");
        }
    }

    #[test]
    fn claude_config_paths_put_relocated_first_and_deduplicate() {
        assert_eq!(
            claude_config_paths(
                Some(Path::new("/home/alice")),
                Some(Path::new("/stores/claude")),
                Path::new(".claude/settings.json"),
                Path::new("settings.json"),
            ),
            [
                PathBuf::from("/stores/claude/settings.json"),
                PathBuf::from("/home/alice/.claude/settings.json"),
            ]
        );
        assert_eq!(
            claude_config_paths(
                Some(Path::new("/home/alice")),
                Some(Path::new("/home/alice/.claude")),
                Path::new(".claude/settings.json"),
                Path::new("settings.json"),
            ),
            [PathBuf::from("/home/alice/.claude/settings.json")]
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_handles_drive_paths() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\C:\Users\me\AppData\Local\ai-memory"),
            r"C:\Users\me\AppData\Local\ai-memory"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\c:\Users\me\AppData\Local\ai-memory"),
            r"c:\Users\me\AppData\Local\ai-memory"
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_handles_unc_case_insensitively() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\UNC\server\share\m"),
            r"\\server\share\m"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\unc\Server\Share\m"),
            r"\\Server\Share\m"
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_leaves_non_verbatim_paths_untouched() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"C:\Users\me\ai-memory"),
            r"C:\Users\me\ai-memory"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix("/home/alice/.local/share/ai-memory"),
            "/home/alice/.local/share/ai-memory"
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_leaves_unknown_forms_untouched() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\m"),
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\m"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\GLOBALROOT\Device\HarddiskVolume1\m"),
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\m"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\.\PhysicalDrive0"),
            r"\\.\PhysicalDrive0"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\C:relative\m"),
            r"\\?\C:relative\m"
        );
    }
}
