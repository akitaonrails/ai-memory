//! `ai-memory-wikisync`: team-wiki sync companion (#986, slices 1 and 3).
//!
//! Keeps explicitly allowlisted page families of a running ai-memory server
//! in step with a directory inside a project repository. Reads go through
//! the public, read-only `/api/v1` surface; the only write to the server is
//! the public `memory_write_page` MCP tool, used by `sync`. The tool never
//! opens the wiki directory or SQLite, never deletes (deletes are slice 4),
//! and never runs git on the operator's behalf.
//!
//! Page bodies are untrusted data: they are transported verbatim into files
//! whose validated paths cannot escape the destination directory, and are
//! never executed or rendered.

pub mod bidi;
pub mod client;
pub mod mcp;
pub mod page_file;
pub mod paths;
pub mod state;
pub mod sync;

/// Hard ceiling on pages handled in one run. A hostile or misconfigured
/// server cannot make the export loop or write unboundedly.
pub const MAX_PAGES: usize = 10_000;
/// Hard ceiling on one page body. The server's own HTTP cap is 10 MiB, so
/// anything near this bound already indicates a broken projection.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Directory (inside `--dest`) holding the single local state file.
pub const STATE_DIR: &str = ".ai-memory-wikisync";
