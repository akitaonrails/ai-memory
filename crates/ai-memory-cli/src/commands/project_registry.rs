//! Client-local checkout links for project-first managed launches.
//!
//! Server project rows intentionally do not expose `repo_path`: that path
//! belongs to the server host and can disclose private filesystem layout. A
//! client records the checkout it successfully used, keyed by the normalized
//! server identity plus `(workspace, project)`, so two machines may map the
//! same remote scope to different local directories.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use ai_memory_core::ManagedProjectScope;
use ai_memory_workstream::inspect_repository_fingerprints;
use anyhow::{Context, Result, bail};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::http_client::ServerEndpoint;

const REGISTRY_VERSION: u32 = 1;
const MAX_REGISTRY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_LINKS: usize = 10_000;
/// Oldest records are evicted first; an evicted session only loses the
/// mismatch check and falls back to the pre-record behavior.
const MAX_SESSION_STORES: usize = 1_000;
const REGISTRY_FILE: &str = "client-projects.json";
const REGISTRY_LOCK_FILE: &str = "client-projects.lock";

/// One server-scoped local checkout link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ProjectLink {
    /// Normalized server origin and mount path, without credentials.
    pub(super) server: String,
    /// Remote workspace name.
    pub(super) workspace: String,
    /// Remote project name.
    pub(super) project: String,
    /// Canonical absolute path on this client.
    pub(super) path: PathBuf,
    /// Last successful managed prepare that refreshed this link.
    pub(super) linked_at: String,
}

/// The native store this client launched one linked session under.
///
/// The server only learns the native session id, so without this record a
/// later launch cannot tell a deleted session from one that lives under a
/// different store override (a second `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SessionStore {
    server: String,
    harness: String,
    native_session_id: String,
    /// Absolute, normalized store root, so the default store and an override
    /// spelling the same directory compare equal.
    store: PathBuf,
    recorded_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProjectRegistry {
    version: u32,
    links: Vec<ProjectLink>,
    // Optional so registries written before this field still load; older
    // binaries ignore it and simply drop it on their next write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    session_stores: Vec<SessionStore>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    bindings: Vec<CheckoutBinding>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CheckoutBinding {
    server: String,
    workspace: String,
    requested_project: String,
    path: PathBuf,
    repo_fingerprint: String,
    worktree_fingerprint: String,
    scope: ManagedProjectScope,
}

impl Default for ProjectRegistry {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            links: Vec::new(),
            session_stores: Vec::new(),
            bindings: Vec::new(),
        }
    }
}

/// Return links belonging to the configured server.
pub(super) fn links_for_server(
    config: &Config,
    endpoint: &ServerEndpoint,
) -> Result<Vec<ProjectLink>> {
    let server = endpoint.identity();
    Ok(load_registry(&registry_path(config))?
        .links
        .into_iter()
        .filter(|link| link.server == server)
        .collect())
}

/// Reuse only a binding for this server, scope and unchanged checkout identity.
/// The server still validates the ID and current permissions on every request.
pub(super) fn project_binding(
    config: &Config,
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    path: &Path,
) -> Result<Option<ManagedProjectScope>> {
    project_binding_for_server(config, &endpoint.identity(), workspace, project, path)
}

pub(super) fn project_binding_for_server(
    config: &Config,
    server: &str,
    workspace: &str,
    project: &str,
    path: &Path,
) -> Result<Option<ManagedProjectScope>> {
    let i18n = &config.i18n;
    let canonical = path
        .canonicalize()
        .context(i18n.text("cli-project-registry-canonicalizing-project-binding-checkout"))?;
    let fingerprints = inspect_repository_fingerprints(&canonical)?;
    let registry = match load_registry(&registry_path(config)) {
        Ok(registry) => registry,
        Err(error) => {
            // A damaged optional registry must not break ordinary checkout
            // discovery. Select no ID and leave the file untouched; the server
            // still refuses ambiguous names and an explicit choice is required.
            eprintln!(
                "{}", i18n.format("cli-project-registry-ai-memory-could-not-read-the-checkout-binding-error-using-guarded-project-na", &[("value0", format!("{:#}", error))]));
            return Ok(None);
        }
    };
    Ok(registry
        .bindings
        .into_iter()
        .find(|binding| {
            binding.server == server
                && binding.workspace == workspace
                && (binding.requested_project == project || binding.scope.project == project)
                && binding.scope.workspace == workspace
                && binding.path == canonical
                && binding.repo_fingerprint == fingerprints.repo_fingerprint
                && binding.worktree_fingerprint == fingerprints.worktree_fingerprint
        })
        .map(|binding| binding.scope))
}

/// Persist only a server-confirmed scope or an explicit authorized selection.
pub(super) fn record_project_binding(
    config: &Config,
    endpoint: &ServerEndpoint,
    requested_project: &str,
    path: &Path,
    scope: &ManagedProjectScope,
    expected: &ai_memory_workstream::RepositoryFingerprints,
) -> Result<()> {
    let i18n = &config.i18n;
    let canonical = path
        .canonicalize()
        .context(i18n.text("cli-project-registry-canonicalizing-project-binding-checkout"))?;
    let fingerprints = inspect_repository_fingerprints(&canonical)?;
    if &fingerprints != expected {
        bail!(
            "{}", i18n.text("cli-project-registry-checkout-identity-changed-while-selecting-the-project-refusing-to-remember-a"));
    }
    let server = endpoint.identity();
    update_registry(config, |registry| {
        // One current choice per checkout and workspace, including a deliberate
        // replacement via --project-id. Never choose the first of stale peers.
        registry.bindings.retain(|binding| {
            !(binding.server == server
                && binding.workspace == scope.workspace
                && binding.path == canonical)
        });
        registry.bindings.push(CheckoutBinding {
            server,
            workspace: scope.workspace.clone(),
            requested_project: requested_project.to_owned(),
            path: canonical,
            repo_fingerprint: fingerprints.repo_fingerprint,
            worktree_fingerprint: fingerprints.worktree_fingerprint,
            scope: scope.clone(),
        });
        Ok(true)
    })
}

/// Record the checkout only after the server accepted a managed run prepare.
///
/// The path is canonicalized before persistence. A later picker rejects a
/// path whose canonical form has changed, which catches a removed checkout
/// replaced by a symlink before any harness is launched.
pub(super) fn record_prepared_checkout(
    config: &Config,
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    path: &Path,
) -> Result<()> {
    let i18n = &config.i18n;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalizing checkout {}", path.display()))?;
    if !canonical.is_dir() {
        bail!(
            "{}",
            i18n.format(
                "cli-project-registry-checkout-is-not-a-directory",
                &[("value0", format!("{}", canonical.display()))]
            )
        );
    }
    let server = endpoint.identity();
    update_registry(config, |registry| {
        registry.links.retain(|link| {
            !(link.server == server && link.workspace == workspace && link.project == project)
        });
        registry.links.push(ProjectLink {
            server,
            workspace: workspace.to_owned(),
            project: project.to_owned(),
            path: canonical,
            linked_at: jiff::Timestamp::now().to_string(),
        });
        Ok(true)
    })
}

/// Move a local link after a successful server-side project rename or move.
///
/// If the destination already has a link, it wins. That is the safe behavior
/// for a copy-and-purge merge into an existing project: the destination's
/// established checkout must not be silently replaced by the source path.
pub(super) fn rekey_scope(
    config: &Config,
    endpoint: &ServerEndpoint,
    from_workspace: &str,
    from_project: &str,
    to_workspace: &str,
    to_project: &str,
) -> Result<()> {
    let server = endpoint.identity();
    update_registry(config, |registry| {
        let Some(source_index) = registry.links.iter().position(|link| {
            link.server == server
                && link.workspace == from_workspace
                && link.project == from_project
        }) else {
            return Ok(false);
        };
        let mut source = registry.links.remove(source_index);
        let destination_exists = registry.links.iter().any(|link| {
            link.server == server && link.workspace == to_workspace && link.project == to_project
        });
        if !destination_exists {
            source.workspace = to_workspace.to_owned();
            source.project = to_project.to_owned();
            registry.links.push(source);
        }
        Ok(true)
    })
}

/// Remember the store a session was linked under, after the server accepted
/// the link (at launch, or at finish for an id discovered after the run).
pub(super) fn record_session_store(
    config: &Config,
    endpoint: &ServerEndpoint,
    harness: &str,
    native_session_id: &str,
    store: &Path,
) -> Result<()> {
    let server = endpoint.identity();
    update_registry(config, |registry| {
        registry.session_stores.retain(|record| {
            !(record.server == server
                && record.harness == harness
                && record.native_session_id == native_session_id)
        });
        registry.session_stores.push(SessionStore {
            server,
            harness: harness.to_owned(),
            native_session_id: native_session_id.to_owned(),
            store: store.to_path_buf(),
            recorded_at: jiff::Timestamp::now().to_string(),
        });
        let excess = registry
            .session_stores
            .len()
            .saturating_sub(MAX_SESSION_STORES);
        registry.session_stores.drain(..excess);
        Ok(true)
    })
}

/// The store a session was linked under, if this client recorded one.
pub(super) fn recorded_session_store(
    config: &Config,
    endpoint: &ServerEndpoint,
    harness: &str,
    native_session_id: &str,
) -> Result<Option<PathBuf>> {
    let server = endpoint.identity();
    Ok(load_registry(&registry_path(config))?
        .session_stores
        .into_iter()
        .find(|record| {
            record.server == server
                && record.harness == harness
                && record.native_session_id == native_session_id
        })
        .map(|record| record.store))
}

fn registry_path(config: &Config) -> PathBuf {
    config.data_dir.join(REGISTRY_FILE)
}

fn lock_path(config: &Config) -> PathBuf {
    config.data_dir.join(REGISTRY_LOCK_FILE)
}

fn update_registry(
    config: &Config,
    mutate: impl FnOnce(&mut ProjectRegistry) -> Result<bool>,
) -> Result<()> {
    let i18n = &config.i18n;
    fs::create_dir_all(&config.data_dir).with_context(|| {
        format!(
            "creating client data directory {}",
            config.data_dir.display()
        )
    })?;
    let lock = open_private_lock(&lock_path(config))?;
    lock.lock_exclusive()
        .context(i18n.text("cli-project-registry-locking-client-project-registry"))?;

    let path = registry_path(config);
    let mut registry = load_registry(&path)?;
    if mutate(&mut registry)? {
        if registry.links.len() > MAX_LINKS || registry.bindings.len() > MAX_LINKS {
            bail!(
                "{}",
                i18n.format(
                    "cli-project-registry-client-project-registry-exceeds-max-links-links",
                    &[("value0", format!("{}", MAX_LINKS))]
                )
            );
        }
        let mut rendered = serde_json::to_vec_pretty(&registry)
            .context(i18n.text("cli-project-registry-serializing-client-project-registry"))?;
        rendered.push(b'\n');
        if rendered.len() as u64 > MAX_REGISTRY_BYTES {
            bail!("{}", i18n.text("cli-project-registry-client-project-registry-exceeds-the-byte-limit-keeping-the-previous-registry"));
        }
        refuse_symlink(&path)?;
        ai_memory_wiki::write_atomic(&path, &rendered)
            .with_context(|| format!("writing client project registry {}", path.display()))?;
        make_private(&path)?;
    }
    Ok(())
}

fn load_registry(path: &Path) -> Result<ProjectRegistry> {
    refuse_symlink(path)?;
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectRegistry::default());
        }
        Err(error) => return Err(error).with_context(|| format!("inspecting {}", path.display())),
    };
    if metadata.len() > MAX_REGISTRY_BYTES {
        bail!(
            "client project registry {} exceeds the {} byte limit",
            path.display(),
            MAX_REGISTRY_BYTES
        );
    }
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let registry: ProjectRegistry = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "parsing {}; refusing to overwrite malformed client state",
            path.display()
        )
    })?;
    if registry.version != REGISTRY_VERSION {
        bail!(
            "unsupported client project registry version {} in {} (expected {})",
            registry.version,
            path.display(),
            REGISTRY_VERSION
        );
    }
    if registry.links.len() > MAX_LINKS || registry.bindings.len() > MAX_LINKS {
        bail!("client project registry exceeds {MAX_LINKS} links");
    }
    let mut keys = HashSet::with_capacity(registry.links.len());
    let mut binding_keys = HashSet::with_capacity(registry.bindings.len());
    for binding in &registry.bindings {
        if !binding_keys.insert((&binding.server, &binding.workspace, &binding.path)) {
            bail!("duplicate checkout project binding; refusing ambiguous client state");
        }
    }
    for link in &registry.links {
        if !keys.insert((&link.server, &link.workspace, &link.project)) {
            bail!(
                "duplicate client project registry key for {}/{}/{} in {}",
                link.server,
                link.workspace,
                link.project,
                path.display()
            );
        }
    }
    Ok(registry)
}

fn open_private_lock(path: &Path) -> Result<File> {
    refuse_symlink(path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("opening client project registry lock {}", path.display()))
}

fn refuse_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "refusing client project registry symlink {}",
                path.display()
            )
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

#[cfg(unix)]
fn make_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn make_private(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_at(path: &Path) -> Config {
        Config {
            data_dir: path.to_path_buf(),
            ..Config::default()
        }
    }

    fn endpoint(url: &str) -> ServerEndpoint {
        ServerEndpoint::from_pair(Some(url.to_owned()), None)
    }

    #[test]
    fn project_id_bindings_survive_reload_and_reject_changed_checkout_or_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        let other = tmp.path().join("other");
        fs::create_dir(&checkout).unwrap();
        fs::create_dir(&other).unwrap();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&checkout)
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        git(&["init"]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://example.test/acme/api.git",
        ]);
        let config = config_at(&tmp.path().join("data"));
        let server = endpoint("http://memory:49374/mount");
        let scope = ManagedProjectScope {
            workspace_id: ai_memory_core::WorkspaceId::new(),
            project_id: ai_memory_core::ProjectId::new(),
            workspace: "default".into(),
            project: "acme-api".into(),
        };
        let fingerprints = inspect_repository_fingerprints(&checkout).unwrap();
        record_project_binding(&config, &server, "api", &checkout, &scope, &fingerprints).unwrap();
        let reloaded = config_at(&tmp.path().join("data"));
        assert_eq!(
            project_binding(&reloaded, &server, "default", "api", &checkout).unwrap(),
            Some(scope.clone())
        );
        assert!(
            project_binding(
                &reloaded,
                &endpoint("http://other:49374/mount"),
                "default",
                "api",
                &checkout
            )
            .unwrap()
            .is_none()
        );
        assert!(
            project_binding(
                &reloaded,
                &endpoint("http://memory:49374/another"),
                "default",
                "api",
                &checkout
            )
            .unwrap()
            .is_none()
        );
        assert!(
            project_binding(&reloaded, &server, "foreign", "api", &checkout)
                .unwrap()
                .is_none()
        );
        assert!(
            project_binding(&reloaded, &server, "default", "different", &checkout)
                .unwrap()
                .is_none()
        );
        assert!(
            project_binding(&reloaded, &server, "default", "api", &other)
                .unwrap()
                .is_none()
        );
        git(&[
            "remote",
            "set-url",
            "origin",
            "https://example.test/other/api.git",
        ]);
        assert!(
            project_binding(&reloaded, &server, "default", "api", &checkout)
                .unwrap()
                .is_none()
        );
        let before = fs::read(registry_path(&config)).unwrap();
        assert!(
            record_project_binding(&config, &server, "api", &checkout, &scope, &fingerprints)
                .is_err()
        );
        assert_eq!(fs::read(registry_path(&config)).unwrap(), before);
        git(&[
            "remote",
            "set-url",
            "origin",
            "https://example.test/acme/api.git",
        ]);
        let replacement = ManagedProjectScope {
            project_id: ai_memory_core::ProjectId::new(),
            ..scope
        };
        record_project_binding(
            &reloaded,
            &server,
            "api",
            &checkout,
            &replacement,
            &fingerprints,
        )
        .unwrap();
        assert_eq!(
            project_binding(&reloaded, &server, "default", "api", &checkout).unwrap(),
            Some(replacement)
        );
        assert_eq!(
            load_registry(&registry_path(&reloaded))
                .unwrap()
                .bindings
                .len(),
            1
        );
    }

    #[test]
    fn project_id_registry_upgrade_and_failed_write_preserve_existing_state() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_at(&tmp.path().join("data"));
        fs::create_dir_all(&config.data_dir).unwrap();
        let old = b"{\"version\":1,\"links\":[]}";
        fs::write(registry_path(&config), old).unwrap();
        assert!(
            load_registry(&registry_path(&config))
                .unwrap()
                .bindings
                .is_empty()
        );
        let error = update_registry(&config, |registry| {
            registry.version = 123;
            bail!("injected failure before atomic replacement")
        });
        assert!(error.is_err());
        assert_eq!(fs::read(registry_path(&config)).unwrap(), old);
    }

    #[test]
    fn links_are_isolated_by_server_and_scope() {
        let tmp = tempfile::TempDir::new().unwrap();
        let checkout = tmp.path().join("checkout");
        fs::create_dir(&checkout).unwrap();
        let config = config_at(&tmp.path().join("data"));
        let first = endpoint("http://memory-one:49374/root/");
        let second = endpoint("http://memory-two:49374");

        record_prepared_checkout(&config, &first, "default", "app", &checkout).unwrap();

        let links = links_for_server(&config, &first).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].path, checkout.canonicalize().unwrap());
        assert!(links_for_server(&config, &second).unwrap().is_empty());
    }

    #[test]
    fn recording_the_same_key_replaces_only_that_link() {
        let tmp = tempfile::TempDir::new().unwrap();
        let first_path = tmp.path().join("first");
        let second_path = tmp.path().join("second");
        let other_path = tmp.path().join("other");
        for path in [&first_path, &second_path, &other_path] {
            fs::create_dir(path).unwrap();
        }
        let config = config_at(&tmp.path().join("data"));
        let endpoint = endpoint("http://memory:49374");

        record_prepared_checkout(&config, &endpoint, "default", "app", &first_path).unwrap();
        record_prepared_checkout(&config, &endpoint, "default", "other", &other_path).unwrap();
        record_prepared_checkout(&config, &endpoint, "default", "app", &second_path).unwrap();

        let links = links_for_server(&config, &endpoint).unwrap();
        assert_eq!(links.len(), 2);
        assert!(links.iter().any(|link| {
            link.project == "app" && link.path == second_path.canonicalize().unwrap()
        }));
        assert!(links.iter().any(|link| link.project == "other"));
    }

    #[test]
    fn malformed_registry_is_not_overwritten() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data = tmp.path().join("data");
        let checkout = tmp.path().join("checkout");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir(&checkout).unwrap();
        let path = data.join(REGISTRY_FILE);
        fs::write(&path, b"not json").unwrap();
        let config = config_at(&data);

        assert!(
            record_prepared_checkout(
                &config,
                &endpoint("http://memory:49374"),
                "default",
                "app",
                &checkout,
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"not json");
    }

    #[test]
    fn duplicate_scope_keys_are_rejected_without_overwriting() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data = tmp.path().join("data");
        let checkout = tmp.path().join("checkout");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir(&checkout).unwrap();
        let path = data.join(REGISTRY_FILE);
        let duplicate = br#"{
  "version": 1,
  "links": [
    {"server":"http://memory:49374","workspace":"default","project":"app","path":"/one","linked_at":"2026-01-01T00:00:00Z"},
    {"server":"http://memory:49374","workspace":"default","project":"app","path":"/two","linked_at":"2026-01-02T00:00:00Z"}
  ]
}
"#;
        fs::write(&path, duplicate).unwrap();
        let config = config_at(&data);

        assert!(links_for_server(&config, &endpoint("http://memory:49374")).is_err());
        assert!(
            record_prepared_checkout(
                &config,
                &endpoint("http://memory:49374"),
                "default",
                "app",
                &checkout,
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), duplicate);
    }

    #[test]
    fn rekey_preserves_an_existing_destination_link() {
        let tmp = tempfile::TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let destination = tmp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        let config = config_at(&tmp.path().join("data"));
        let endpoint = endpoint("http://memory:49374");
        record_prepared_checkout(&config, &endpoint, "from", "app", &source).unwrap();
        record_prepared_checkout(&config, &endpoint, "to", "app", &destination).unwrap();

        rekey_scope(&config, &endpoint, "from", "app", "to", "app").unwrap();

        let links = links_for_server(&config, &endpoint).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].path, destination.canonicalize().unwrap());
    }

    #[test]
    fn rekey_preserves_the_source_link_timestamp() {
        let tmp = tempfile::TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir(&source).unwrap();
        let config = config_at(&tmp.path().join("data"));
        let endpoint = endpoint("http://memory:49374");
        record_prepared_checkout(&config, &endpoint, "from", "app", &source).unwrap();
        let original_linked_at = "2025-01-02T03:04:05Z";
        update_registry(&config, |registry| {
            registry.links[0].linked_at = original_linked_at.to_owned();
            Ok(true)
        })
        .unwrap();

        rekey_scope(&config, &endpoint, "from", "app", "to", "renamed").unwrap();

        let links = links_for_server(&config, &endpoint).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].workspace, "to");
        assert_eq!(links[0].project, "renamed");
        assert_eq!(links[0].linked_at, original_linked_at);
    }

    #[test]
    fn session_stores_are_keyed_by_server_harness_and_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = config_at(&tmp.path().join("data"));
        let first = endpoint("http://memory-one:49374");
        let second = endpoint("http://memory-two:49374");
        let custom = tmp.path().join("claude-glm").join("projects");
        let default = tmp.path().join("home").join(".claude").join("projects");

        record_session_store(&config, &first, "claude", "abc", &custom).unwrap();
        record_session_store(&config, &first, "codex", "def", &default).unwrap();

        assert_eq!(
            recorded_session_store(&config, &first, "claude", "abc").unwrap(),
            Some(custom.clone())
        );
        assert_eq!(
            recorded_session_store(&config, &first, "codex", "def").unwrap(),
            Some(default.clone())
        );
        assert_eq!(
            recorded_session_store(&config, &first, "codex", "abc").unwrap(),
            None
        );
        assert_eq!(
            recorded_session_store(&config, &second, "claude", "abc").unwrap(),
            None
        );

        record_session_store(&config, &first, "claude", "abc", &default).unwrap();
        assert_eq!(
            recorded_session_store(&config, &first, "claude", "abc").unwrap(),
            Some(default)
        );
    }

    /// Registries written before session stores existed keep loading, and
    /// recording one keeps their links.
    #[test]
    fn registries_without_session_stores_still_load() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data = tmp.path().join("data");
        let checkout = tmp.path().join("checkout");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir(&checkout).unwrap();
        let config = config_at(&data);
        let endpoint = endpoint("http://memory:49374");
        let store = tmp.path().join("store");
        record_prepared_checkout(&config, &endpoint, "default", "app", &checkout).unwrap();
        let legacy = fs::read_to_string(data.join(REGISTRY_FILE)).unwrap();
        assert!(!legacy.contains("session_stores"), "{legacy}");

        assert_eq!(
            recorded_session_store(&config, &endpoint, "claude", "abc").unwrap(),
            None
        );
        record_session_store(&config, &endpoint, "claude", "abc", &store).unwrap();

        assert_eq!(links_for_server(&config, &endpoint).unwrap().len(), 1);
        assert_eq!(
            recorded_session_store(&config, &endpoint, "claude", "abc").unwrap(),
            Some(store)
        );
    }

    #[test]
    fn session_store_records_evict_the_oldest_first() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = config_at(&tmp.path().join("data"));
        let endpoint = endpoint("http://memory:49374");
        let server = endpoint.identity();
        let store = tmp.path().join("store");
        update_registry(&config, |registry| {
            registry.session_stores = (0..MAX_SESSION_STORES)
                .map(|index| SessionStore {
                    server: server.clone(),
                    harness: "claude".to_owned(),
                    native_session_id: format!("session-{index}"),
                    store: store.clone(),
                    recorded_at: "2026-01-01T00:00:00Z".to_owned(),
                })
                .collect();
            Ok(true)
        })
        .unwrap();

        record_session_store(&config, &endpoint, "claude", "newest", &store).unwrap();

        let path = registry_path(&config);
        assert_eq!(
            load_registry(&path).unwrap().session_stores.len(),
            MAX_SESSION_STORES
        );
        assert_eq!(
            recorded_session_store(&config, &endpoint, "claude", "session-0").unwrap(),
            None
        );
        assert_eq!(
            recorded_session_store(&config, &endpoint, "claude", "newest").unwrap(),
            Some(store)
        );
    }
}
