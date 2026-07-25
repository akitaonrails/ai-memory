//! Wiki filesystem layer.
//!
//! Owns the markdown-on-disk source of truth: atomic writes, frontmatter
//! parsing/emission, and write-through to the [`ai_memory_store`] writer
//! actor so the SQLite index never diverges from the file. The watcher +
//! git layer arrive in M1-D and M5.

pub mod admission;
mod atomic;
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
pub use wiki::{Wiki, WritePageRequest};
