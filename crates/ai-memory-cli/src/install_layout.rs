//! Release and install path basenames shared by packaging-aware CLI commands.
//!
//! These names match the shipped binary (`[[bin]] name`), release archive
//! entries, and the binary-sibling hooks bundle. They are not domain
//! constants — keep them in the CLI crate, not `ai-memory-core`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Shipped binary basename (`[[bin]] name`, `/proc/*/comm`).
///
/// Release archives use [`shipped_binary_name`] instead — Windows zips ship
/// `ai-memory.exe`.
pub const BINARY_NAME: &str = "ai-memory";

/// Sibling hooks bundle dir in release archives and install prefixes.
pub const HOOKS_DIR_NAME: &str = "hooks";

/// On-disk / archive basename for the release binary on this host.
///
/// Matches `release.yml`: Unix tarballs ship `ai-memory`; the Windows zip
/// ships `ai-memory.exe`.
pub fn shipped_binary_name() -> &'static str {
    #[cfg(windows)]
    {
        "ai-memory.exe"
    }
    #[cfg(not(windows))]
    {
        BINARY_NAME
    }
}

/// Scoop's per-app directory name for ai-memory (`scoop install ai-memory`).
pub const SCOOP_APP_NAME: &str = "ai-memory";

/// Scoop's version-independent directory name: `<root>\apps\<app>\current` is
/// a junction to the active `<root>\apps\<app>\<version>`, and the shims on
/// `PATH` run the binary through it.
const SCOOP_CURRENT_DIR: &str = "current";

/// The `current` links for a file inside a versioned Scoop app directory,
/// `<root>/apps/<app>/<version>/<rest>` → `<root>/apps/<app>/current/<rest>`.
///
/// `std::fs::canonicalize` resolves the `current` junction, so a path recorded
/// from it names the version that `scoop update` and `scoop cleanup` remove.
/// The match is lexical, one candidate per `apps` component, and callers
/// confirm a candidate exists before using it. A path already under `current`
/// yields nothing.
pub(crate) fn scoop_current_links(path: &Path) -> Vec<PathBuf> {
    let parts: Vec<_> = path.components().map(|part| part.as_os_str()).collect();
    scoop_app_indexes(&parts)
        .filter(|&index| !eq_ignore_case(parts[index + 2], SCOOP_CURRENT_DIR))
        .map(|index| {
            let mut link: PathBuf = parts[..index + 2].iter().collect();
            link.push(SCOOP_CURRENT_DIR);
            link.extend(&parts[index + 3..]);
            link
        })
        .collect()
}

/// Whether `path` names a file inside Scoop's `<root>/apps/<app>/<dir>/` for
/// `app`, through either a version directory or the `current` link.
pub(crate) fn is_scoop_app_path(path: &Path, app: &str) -> bool {
    let parts: Vec<_> = path.components().map(|part| part.as_os_str()).collect();
    scoop_app_indexes(&parts).any(|index| eq_ignore_case(parts[index + 1], app))
}

/// Indexes of `apps` components followed by `<app>/<dir>/<file...>`.
fn scoop_app_indexes<'a>(parts: &'a [&'a OsStr]) -> impl Iterator<Item = usize> + 'a {
    parts
        .iter()
        .enumerate()
        .filter(move |(index, part)| eq_ignore_case(part, "apps") && parts.len() > index + 3)
        .map(|(index, _)| index)
}

/// Windows paths, where Scoop lives, compare without case.
fn eq_ignore_case(part: &OsStr, name: &str) -> bool {
    part.to_str()
        .is_some_and(|part| part.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(parts: &[&str]) -> PathBuf {
        parts.iter().collect()
    }

    #[test]
    fn scoop_version_dir_maps_to_current_link() {
        let root = path(&["/", "Users", "me", "scoop"]);
        let versioned = root.join("apps/ai-memory/3.4.0/ai-memory.exe");
        assert_eq!(
            scoop_current_links(&versioned),
            vec![root.join("apps/ai-memory/current/ai-memory.exe")]
        );
        let nested = root.join("apps/ai-memory/3.4.0/bin/ai-memory.exe");
        assert_eq!(
            scoop_current_links(&nested),
            vec![root.join("apps/ai-memory/current/bin/ai-memory.exe")]
        );
    }

    #[test]
    fn scoop_current_link_and_unrelated_paths_have_no_links() {
        for unrelated in [
            "/Users/me/scoop/apps/ai-memory/current/ai-memory.exe",
            "/Users/me/scoop/apps/ai-memory/CURRENT/ai-memory.exe",
            "/Users/me/.local/bin/ai-memory",
            // `apps/<app>/<file>` has no version directory.
            "/Users/me/scoop/apps/ai-memory/ai-memory.exe",
            "/usr/local/bin/ai-memory",
        ] {
            assert!(
                scoop_current_links(Path::new(unrelated)).is_empty(),
                "{unrelated}"
            );
        }
    }

    #[test]
    fn scoop_match_ignores_case_like_windows() {
        let versioned = Path::new("/Users/me/Scoop/Apps/AI-Memory/3.4.0/ai-memory.exe");
        assert_eq!(
            scoop_current_links(versioned),
            vec![PathBuf::from(
                "/Users/me/Scoop/Apps/AI-Memory/current/ai-memory.exe"
            )]
        );
        assert!(is_scoop_app_path(versioned, SCOOP_APP_NAME));
    }

    #[test]
    fn scoop_app_path_names_only_the_given_app() {
        for scoop in [
            "/Users/me/scoop/apps/ai-memory/3.4.0/ai-memory.exe",
            "/Users/me/scoop/apps/ai-memory/current/ai-memory.exe",
            "/ProgramData/scoop/apps/ai-memory/3.4.0/ai-memory.exe",
        ] {
            assert!(
                is_scoop_app_path(Path::new(scoop), SCOOP_APP_NAME),
                "{scoop}"
            );
        }
        for other in [
            "/Users/me/scoop/apps/other-tool/1.0/ai-memory.exe",
            "/Users/me/scoop/apps/ai-memory/ai-memory.exe",
            "/Users/me/.local/bin/ai-memory",
        ] {
            assert!(
                !is_scoop_app_path(Path::new(other), SCOOP_APP_NAME),
                "{other}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn scoop_links_keep_a_windows_drive_prefix() {
        let versioned = Path::new(r"C:\Users\me\scoop\apps\ai-memory\3.4.0\ai-memory.exe");
        assert_eq!(
            scoop_current_links(versioned),
            vec![PathBuf::from(
                r"C:\Users\me\scoop\apps\ai-memory\current\ai-memory.exe"
            )]
        );
        let verbatim = Path::new(r"\\?\C:\Users\me\scoop\apps\ai-memory\3.4.0\ai-memory.exe");
        assert_eq!(
            scoop_current_links(verbatim),
            vec![PathBuf::from(
                r"\\?\C:\Users\me\scoop\apps\ai-memory\current\ai-memory.exe"
            )]
        );
        assert!(is_scoop_app_path(verbatim, SCOOP_APP_NAME));
    }
}
