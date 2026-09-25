//! Web router state — the handle a request handler receives.
//!
//! Holds the read-only store pool + the wiki handle. Cheap to clone
//! (everything inside is `Arc`-shaped already), so axum's
//! `State<Arc<WebState>>` extractor stays free of clone-heavy code.

use ai_memory_store::ReaderPool;
use ai_memory_wiki::Wiki;

/// Shared state for every web route. Construct once via
/// [`crate::router`].
#[derive(Clone)]
pub struct WebState {
    /// Read-only SQLite pool — drives FTS5 search, page metadata,
    /// project list aggregates.
    pub reader: ReaderPool,
    /// Wiki handle — reads page bodies from disk.
    pub wiki: Wiki,
    /// A trusted identity proxy asserts usernames, so the deployment tells
    /// operators apart even with no `users` rows. Root-only pages pass it to
    /// `ReaderPool::distinguishes_operators`, as the `/admin` gate does.
    pub trusted_proxy_identity: bool,
}

impl WebState {
    /// Build a new shared state.
    #[must_use]
    pub fn new(reader: ReaderPool, wiki: Wiki) -> Self {
        Self {
            reader,
            wiki,
            trusted_proxy_identity: false,
        }
    }

    /// Record whether a trusted identity proxy is configured.
    #[must_use]
    pub fn with_trusted_proxy_identity(mut self, trusted_proxy_identity: bool) -> Self {
        self.trusted_proxy_identity = trusted_proxy_identity;
        self
    }
}
