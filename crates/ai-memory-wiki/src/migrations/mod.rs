//! Wiki-structure migration framework.
//!
//! SQL schema migrations are handled by `refinery` and run before this
//! layer is invoked. Wiki migrations handle *filesystem* changes — file
//! moves, path rewrites, directory renames — that are required when the
//! on-disk wiki layout changes between versions.
//!
//! ## Adding a new migration
//!
//! 1. Create a struct in a new submodule (e.g. `crates/ai-memory-wiki/src/migrations/m2026_05_24_per_project_layout.rs`).
//! 2. Implement [`WikiMigration`] for it.
//! 3. Add an instance to the `vec![]` in [`registry`]. **Always append at the
//!    end — never reorder or remove entries.**
//! 4. Add a test in your module that exercises the migration against a tmp
//!    wiki directory.
//!
//! ## Naming convention
//!
//! Migration names use `YYYY_MM_DDTHH_MM_<descriptive_snake_case>`. The
//! timestamp is UTC and chosen at authoring time, not applied time. Using a
//! timestamp rather than a sequence number avoids merge conflicts when two
//! contributors add migrations in parallel.
//!
//! ## Idempotency
//!
//! Every implementation of [`WikiMigration::up`] must be idempotent: if the
//! work is already done (files already in the target layout, target dir
//! already absent, etc.), the migration exits with `Ok(())` without touching
//! anything. The runner marks it applied on first success and never re-runs
//! it, but a correct implementation guards against the invariant anyway.
//!
//! ## What NOT to do
//!
//! - **No destructive deletes without a graveyard step.** Move the file to
//!   `<wiki_root>/_graveyard/<timestamp>/<original_path>` before deleting so
//!   data is recoverable for at least one release cycle.
//! - **No LLM calls.** Migrations run on every server start; they must be
//!   fast and free.
//! - **No SQL outside the writer actor.** Use [`WriterHandle`] methods so all
//!   writes go through the single-writer channel (invariant #2).

mod m2026_09_okf_conformance;
mod runner;

use std::path::Path;

use ai_memory_store::WriterHandle;

pub use m2026_09_okf_conformance::snapshot_before_db_migration;
pub use runner::run_pending;

use crate::error::WikiResult;

/// A single wiki-structure migration.
///
/// Implementors describe one idempotent filesystem (and optional SQL)
/// transformation. The runner calls [`up`](WikiMigration::up) exactly once
/// per data directory, tracking completion in the `wiki_migrations` table.
#[async_trait::async_trait]
pub trait WikiMigration: Send + Sync {
    /// Unique, sortable name. Convention: `YYYY_MM_DDTHH_MM_<descriptive_snake>`.
    ///
    /// This value is stored in the `wiki_migrations` table as the primary key.
    /// Once chosen and shipped it must never change.
    fn name(&self) -> &'static str;

    /// One-line description shown in server logs.
    fn description(&self) -> &'static str;

    /// Apply the migration.
    ///
    /// The `writer` handle is provided so any SQL updates that accompany the
    /// file moves go through the single-writer actor (no race with hooks).
    /// `wiki_root` is the on-disk root of the wiki directory
    /// (`<data_dir>/wiki/`).
    ///
    /// Implementations **must** be idempotent: if the work has already been
    /// done they return `Ok(())` immediately.
    ///
    /// # Errors
    ///
    /// Any error returned here causes the server to bail at startup. The
    /// migration row is NOT inserted into `wiki_migrations`, so the next start
    /// will retry.
    async fn up(&self, writer: &WriterHandle, wiki_root: &Path) -> WikiResult<()>;
}

/// The canonical migration registry.
///
/// Migrations run sequentially in the order they appear. **Always append at
/// the end; never reorder or remove entries.** The runner uses this order
/// together with the `wiki_migrations` table to determine which entries are
/// still pending.
///
/// v1 ships with no pending migrations: the per-project UUID-namespaced
/// layout is the native format from day one and requires no transformation
/// of pre-existing data.
#[must_use]
pub fn registry() -> Vec<Box<dyn WikiMigration>> {
    vec![Box::new(
        m2026_09_okf_conformance::OkfConformance::from_env(),
    )]
}
