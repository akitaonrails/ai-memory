//! Content-backend adapters compiled into this crate.
//!
//! `[storage] backend = "<name>"` selects either the built-in
//! `"sqlite"` pipeline (markdown wiki + SQLite index — not listed
//! here) or one of the adapters below. Each adapter lives in its own
//! module gated by a cargo feature, so consumers that only need the
//! default never build the adapter's dependencies.
//!
//! To ship a new adapter:
//!
//! 1. add a module under `src/adapters/<name>/` implementing
//!    [`crate::ContentBackend`] + [`crate::ContentBackendFactory`]
//!    (see [`self::outl`] for the reference implementation and
//!    `docs/storage-adapters.md` for the contract);
//! 2. gate it behind a feature (`adapter-<name>`) carrying its extra
//!    dependencies;
//! 3. list its factory in [`registry`].

use crate::ContentBackendFactory;

#[cfg(feature = "adapter-outl")]
pub mod outl;

/// Every compiled-in content-backend adapter.
#[must_use]
pub fn registry() -> Vec<Box<dyn ContentBackendFactory>> {
    let mut factories: Vec<Box<dyn ContentBackendFactory>> = Vec::new();
    #[cfg(feature = "adapter-outl")]
    factories.push(Box::new(outl::OutlAdapterFactory));
    factories
}

/// Find an adapter by its `[storage] backend` name.
#[must_use]
pub fn find(name: &str) -> Option<Box<dyn ContentBackendFactory>> {
    registry().into_iter().find(|f| f.name() == name)
}

/// Names of every registered adapter (for error messages).
#[must_use]
pub fn names() -> Vec<&'static str> {
    registry().iter().map(|f| f.name()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_names_are_unique_and_never_shadow_the_default() {
        let all = names();
        let mut deduped = all.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(deduped.len(), all.len(), "duplicate adapter names");
        assert!(
            !all.contains(&"sqlite"),
            "an adapter must not shadow the built-in default"
        );
    }

    #[cfg(feature = "adapter-outl")]
    #[test]
    fn outl_adapter_is_registered() {
        assert!(find("outl").is_some());
        assert!(find("nope").is_none());
    }
}
