//! Wiki filesystem layer.
//!
//! Owns the markdown-on-disk source of truth: atomic writes, frontmatter
//! parsing/emission, and write-through to the [`ai_memory_store`] writer
//! actor so the SQLite index never diverges from the file. The watcher +
//! git layer arrive in M1-D and M5.

pub mod admission;
mod atomic;
pub mod backup;
mod error;
mod git;
mod markdown;
pub mod migrations;
mod watcher;
mod wiki;

pub use admission::{
    AdmissionChain, AdmissionContext, AdmissionOp, FailurePolicy, MAX_ADMISSION_WEBHOOKS,
    MAX_RESPONSE_BYTES, WebhookConfig,
};
pub use atomic::write_atomic;
pub use error::{WikiError, WikiResult};
pub use git::{COMMIT_AUTHOR_EMAIL, COMMIT_AUTHOR_NAME, GitAdapter};
pub use markdown::{Markdown, derive_title, emit, parse};
pub use migrations::run_pending as run_wiki_migrations;
pub use watcher::{DEBOUNCE_WINDOW, RECONCILE_INTERVAL, WatcherHandle};
pub use wiki::{MoveSessionOutcome, SessionPageFile, Wiki, WritePageRequest};

// Integration tests compile into this crate's test harness instead of a
// separate binary: every test binary is another link and, on macOS and
// Windows, another first-run malware scan. They still exercise only the
// public API; `extern crate self` lets them keep addressing it by crate name.
#[cfg(test)]
extern crate self as ai_memory_wiki;
#[cfg(test)]
#[path = "../tests/suite/mod.rs"]
mod integration;
