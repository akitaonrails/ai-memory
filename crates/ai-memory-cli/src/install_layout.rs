//! Release and install path basenames shared by packaging-aware CLI commands.
//!
//! These names match the shipped binary (`[[bin]] name`), release archive
//! entries, and the binary-sibling hooks bundle. They are not domain
//! constants — keep them in the CLI crate, not `ai-memory-core`.

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
