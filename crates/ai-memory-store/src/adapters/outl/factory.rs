//! [`ContentBackendFactory`] implementation — the plug-in point
//! [`crate::adapters::registry`] exposes at boot. Parses the
//! `[storage.outl]` section, opens the workspace, and (in primary
//! mode) spawns the reconciler.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::{AdapterContext, BuiltBackend, ContentBackendFactory, ContentError, ContentResult};
use async_trait::async_trait;
use serde::Deserialize;

use super::{OutlContentBackend, ShadowContentBackend, reconcile};

/// How the Outl backend participates in writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutlMode {
    /// Outl is the content source of truth; the ops reconciler heals
    /// the index from external edits.
    #[default]
    Primary,
    /// Migration aid: the built-in fs pipeline stays primary and every
    /// write is mirrored into the Outl workspace best-effort.
    Shadow,
}

/// `[storage.outl]` settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutlAdapterSettings {
    /// Path of the Outl workspace directory (the one holding `.outl/`,
    /// `ops/`, `pages/`). `~` expands to the operator home.
    pub workspace_dir: PathBuf,
    /// Slug namespace for pages written by ai-memory (default
    /// `ai-memory`).
    #[serde(default)]
    pub slug_prefix: Option<String>,
    /// `primary` (default) or `shadow`.
    #[serde(default)]
    pub mode: OutlMode,
    /// Reconcile scan interval in seconds (primary mode only).
    #[serde(default = "default_reconcile_secs")]
    pub reconcile_secs: u64,
}

fn default_reconcile_secs() -> u64 {
    5
}

impl OutlAdapterSettings {
    /// Effective slug prefix.
    #[must_use]
    pub fn slug_prefix(&self) -> &str {
        self.slug_prefix
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("ai-memory")
    }
}

fn expand_home(p: &std::path::Path, home: &Option<String>) -> PathBuf {
    let (Some(raw), Some(home)) = (p.to_str(), home) else {
        return p.to_path_buf();
    };
    if raw == "~" {
        return PathBuf::from(home);
    }
    match raw.strip_prefix("~/") {
        Some(rest) => PathBuf::from(home).join(rest),
        None => p.to_path_buf(),
    }
}

/// Factory listed in [`crate::adapters::registry`] behind the
/// `adapter-outl` feature.
pub struct OutlAdapterFactory;

#[async_trait]
impl ContentBackendFactory for OutlAdapterFactory {
    fn name(&self) -> &'static str {
        "outl"
    }

    async fn build(&self, ctx: AdapterContext) -> ContentResult<BuiltBackend> {
        if ctx.settings.is_null() {
            return Err(ContentError::Backend(
                "[storage] backend = \"outl\" requires a [storage.outl] section with \
                 workspace_dir"
                    .into(),
            ));
        }
        let settings: OutlAdapterSettings = serde_json::from_value(ctx.settings.clone())
            .map_err(|e| ContentError::Backend(format!("[storage.outl]: {e}")))?;
        let workspace_dir = expand_home(&settings.workspace_dir, &ctx.home_dir);

        let (backend, info) = OutlContentBackend::open(
            workspace_dir.clone(),
            settings.slug_prefix(),
            ctx.writer.clone(),
            ctx.reader.clone(),
        )?;
        tracing::info!(
            workspace = %workspace_dir.display(),
            actor = %info.actor,
            ephemeral_actor = info.ephemeral_actor,
            mode = ?settings.mode,
            "outl content backend attached"
        );

        match settings.mode {
            OutlMode::Primary => {
                let reconciler = reconcile::spawn(
                    backend.clone(),
                    Duration::from_secs(settings.reconcile_secs),
                );
                Ok(BuiltBackend {
                    backend,
                    tasks: vec![reconciler],
                })
            }
            OutlMode::Shadow => Ok(BuiltBackend {
                backend: Arc::new(ShadowContentBackend::new(ctx.fs_backend, backend)),
                tasks: Vec::new(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_with_defaults() {
        let s: OutlAdapterSettings =
            serde_json::from_value(serde_json::json!({"workspace_dir": "/tmp/x"})).unwrap();
        assert_eq!(s.slug_prefix(), "ai-memory");
        assert_eq!(s.mode, OutlMode::Primary);
        assert_eq!(s.reconcile_secs, 5);
    }

    #[test]
    fn settings_reject_unknown_keys() {
        let err = serde_json::from_value::<OutlAdapterSettings>(
            serde_json::json!({"workspace_dir": "/tmp/x", "typo_key": 1}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("typo_key"));
    }

    #[test]
    fn settings_require_workspace_dir() {
        assert!(serde_json::from_value::<OutlAdapterSettings>(serde_json::json!({})).is_err());
    }

    #[test]
    fn home_expansion() {
        let home = Some("/Users/x".to_string());
        assert_eq!(
            expand_home(std::path::Path::new("~/notes"), &home),
            PathBuf::from("/Users/x/notes")
        );
        assert_eq!(
            expand_home(std::path::Path::new("/abs/notes"), &home),
            PathBuf::from("/abs/notes")
        );
    }
}
