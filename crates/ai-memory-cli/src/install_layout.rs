//! Release and install path basenames shared by packaging-aware CLI commands.
//!
//! These names match the shipped binary (`[[bin]] name`), release tarball
//! entries, and the binary-sibling hooks bundle. They are not domain
//! constants — keep them in the CLI crate, not `ai-memory-core`.

/// Shipped binary basename (`[[bin]] name`, `/proc/*/comm`, release tarball entry).
pub const BINARY_NAME: &str = "ai-memory";

/// Sibling hooks bundle dir in release archives and install prefixes.
pub const HOOKS_DIR_NAME: &str = "hooks";
