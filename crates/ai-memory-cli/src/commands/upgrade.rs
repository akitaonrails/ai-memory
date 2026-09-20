//! `ai-memory upgrade` — refresh a GitHub-release native install.
//!
//! Docker-wrapper installs never reach this path: [`bin/ai-memory`] intercepts
//! `upgrade` and pulls the container image. This command covers the release
//! binary laid down under a writable user prefix (the macOS happy path in
//! `docs/macos.md`): download the matching tarball + `.sha256`, verify,
//! atomically replace the on-disk binary (and sibling `hooks/` when present),
//! then re-run `install-hooks --apply` for every staged agent.
//!
//! Client-only: never claims to upgrade a remote/homelab server.
//!
//! Ownership (live CLI command — kept in one module by convention):
//! - install classification (container / package-managed / writable)
//! - release fetch + checksum
//! - archive extract + path allowlist
//! - atomic binary/dir replace
//! - staged hook refresh

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tracing::info;

use crate::cli::{AgentChoice, InstallHooksArgs, UpgradeArgs};
use crate::commands::install_hooks;
use crate::config::Config;
use crate::install_layout::{BINARY_NAME, HOOKS_DIR_NAME};

const RELEASE_OWNER_REPO: &str = "akitaonrails/ai-memory";
const USER_AGENT: &str = concat!("ai-memory-cli/", env!("CARGO_PKG_VERSION"));
/// Hard cap on any single upgrade HTTP body (release tarball, `.sha256`,
/// or tag JSON). Current release archives are ~15–17 MiB; 128 MiB leaves
/// headroom while refusing multi-GB DoS if a mirror or compromised host
/// advertises / streams an oversized payload.
const MAX_RELEASE_DOWNLOAD_BYTES: usize = 128 * 1024 * 1024;
const HTTP_TIMEOUT_SECS: u64 = 120;
#[cfg(unix)]
const UNIX_EXECUTABLE_MODE: u32 = 0o755;

/// Known agent dir + parsed choice pairs from a staged hooks root.
type StagedAgentList = Vec<(String, AgentChoice)>;

/// Run the `upgrade` subcommand.
pub async fn run(config: &Config, args: UpgradeArgs) -> Result<()> {
    #[cfg(windows)]
    {
        let _ = (config, args);
        bail!(
            "native `ai-memory upgrade` does not yet support Windows self-replace; \
             download the release zip from \
             https://github.com/{RELEASE_OWNER_REPO}/releases/latest \
             and re-run `install-hooks --apply` for each agent"
        );
    }

    #[cfg(not(windows))]
    {
        run_unix(config, args).await
    }
}

#[cfg(not(windows))]
async fn run_unix(config: &Config, args: UpgradeArgs) -> Result<()> {
    let exe = resolve_current_exe()?;
    ensure_supported_install(&exe)?;

    let fetcher = ReqwestFetcher::new()?;
    let base = release_base_url(config);
    let tag = resolve_tag(&fetcher, &base, args.version.as_deref()).await?;
    if skip_when_current(&tag, args.force) {
        return Ok(());
    }

    let extract_root = download_and_extract_release(&fetcher, &base, &tag).await?;
    apply_extracted_release(extract_root.path(), &exe)?;

    // Sync FS after await is intentional for this CLI one-shot path.
    refresh_staged_hooks(config)?;
    warn_remote_server(config);
    println!("✓ upgraded to {tag}");
    info!(%tag, path = %exe.display(), "native upgrade complete");
    Ok(())
}

#[cfg(not(windows))]
fn resolve_current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("resolving current executable path")?;
    Ok(fs::canonicalize(&exe).unwrap_or(exe))
}

#[cfg(not(windows))]
fn ensure_supported_install(exe: &Path) -> Result<()> {
    match classify_install(exe)? {
        InstallClass::Supported => Ok(()),
        InstallClass::Unsupported(reason) => bail!("{reason}"),
    }
}

#[cfg(not(windows))]
fn skip_when_current(tag: &str, force: bool) -> bool {
    let current = env!("CARGO_PKG_VERSION");
    if !force && versions_match(tag, current) {
        println!("already up to date ({current})");
        return true;
    }
    false
}

#[cfg(not(windows))]
async fn download_and_extract_release(
    fetcher: &ReqwestFetcher,
    base: &str,
    tag: &str,
) -> Result<tempfile::TempDir> {
    let asset = release_asset_name().context("no GitHub release asset for this OS/arch")?;
    let archive_bytes = fetch_verified_archive(fetcher, base, tag, asset).await?;
    let extract_root = tempfile::tempdir().context("creating extract temp dir")?;
    extract_release_archive(&archive_bytes, extract_root.path())
        .context("extracting release archive")?;
    ensure_extracted_binary(extract_root.path())?;
    Ok(extract_root)
}

#[cfg(not(windows))]
async fn fetch_verified_archive(
    fetcher: &ReqwestFetcher,
    base: &str,
    tag: &str,
    asset: &str,
) -> Result<Vec<u8>> {
    let archive_url = format!("{base}/download/{tag}/{asset}");
    let checksum_url = format!("{archive_url}.sha256");
    println!("→ downloading {asset} ({tag})");
    let archive_bytes = fetcher
        .get_bytes(&archive_url)
        .await
        .with_context(|| format!("downloading {archive_url}"))?;
    let checksum_text = fetcher
        .get_text(&checksum_url)
        .await
        .with_context(|| format!("downloading {checksum_url}"))?;
    verify_archive_checksum(&archive_bytes, &checksum_text, asset)?;
    Ok(archive_bytes)
}

fn ensure_extracted_binary(extract_root: &Path) -> Result<()> {
    if extract_root.join(BINARY_NAME).is_file() {
        return Ok(());
    }
    bail!("release archive is missing the {BINARY_NAME} binary");
}

fn verify_archive_checksum(archive_bytes: &[u8], checksum_text: &str, asset: &str) -> Result<()> {
    let expected = parse_sha256_sidecar(checksum_text, asset)
        .with_context(|| format!("parsing checksum sidecar for {asset}"))?;
    let actual = sha256_hex(archive_bytes);
    if !actual.eq_ignore_ascii_case(&expected) {
        bail!("release archive checksum mismatch (expected {expected}, got {actual})");
    }
    println!("  ✓ checksum ok");
    Ok(())
}

#[cfg(not(windows))]
fn apply_extracted_release(extract_root: &Path, exe: &Path) -> Result<()> {
    let install_dir = exe
        .parent()
        .map(Path::to_path_buf)
        .context("executable has no parent directory")?;
    replace_binary(extract_root, exe)?;
    maybe_refresh_sibling_hooks(extract_root, &install_dir)?;
    Ok(())
}

#[cfg(not(windows))]
fn replace_binary(extract_root: &Path, exe: &Path) -> Result<()> {
    let new_binary = extract_root.join(BINARY_NAME);
    println!("→ replacing {}", exe.display());
    replace_file_atomic(&new_binary, exe).with_context(|| {
        format!(
            "replacing {}; if this fails mid-way look for {}.new",
            exe.display(),
            exe.display()
        )
    })
}

fn maybe_refresh_sibling_hooks(extract_root: &Path, install_dir: &Path) -> Result<()> {
    let extracted_hooks = extract_root.join(HOOKS_DIR_NAME);
    let sibling_hooks = install_dir.join(HOOKS_DIR_NAME);
    if !(extracted_hooks.is_dir() && sibling_hooks.exists()) {
        return Ok(());
    }
    println!("→ refreshing {}", sibling_hooks.display());
    replace_dir_atomic(&extracted_hooks, &sibling_hooks)
        .with_context(|| format!("replacing {}", sibling_hooks.display()))
}

#[derive(Debug, PartialEq, Eq)]
enum InstallClass {
    Supported,
    Unsupported(String),
}

fn classify_install(exe: &Path) -> Result<InstallClass> {
    if let Some(reason) = container_refusal() {
        return Ok(InstallClass::Unsupported(reason));
    }
    if let Some(reason) = package_managed_refusal(exe) {
        return Ok(InstallClass::Unsupported(reason));
    }
    if let Some(reason) = unwritable_refusal(exe)? {
        return Ok(InstallClass::Unsupported(reason));
    }
    Ok(InstallClass::Supported)
}

fn container_refusal() -> Option<String> {
    if Path::new("/.dockerenv").exists() {
        Some(
            "refusing to self-upgrade inside a container; run `ai-memory upgrade` on the \
             host Docker wrapper, or replace the image with `docker pull`"
                .into(),
        )
    } else {
        None
    }
}

fn package_managed_refusal(exe: &Path) -> Option<String> {
    let path_str = exe.to_string_lossy();
    for prefix in package_managed_prefixes() {
        if path_str.starts_with(prefix) {
            return Some(format!(
                "refusing to self-upgrade a package-managed install at {}; \
                 use your package manager (Homebrew, AUR, apt, …) instead",
                exe.display()
            ));
        }
    }
    None
}

fn unwritable_refusal(exe: &Path) -> Result<Option<String>> {
    let parent = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("executable has no parent directory"))?;
    if is_writable_dir(parent) && is_writable_file(exe) {
        return Ok(None);
    }
    Ok(Some(format!(
        "refusing to self-upgrade: {} is not writable by this user; \
         install under ~/.local (or another user-owned prefix) and re-run",
        exe.display()
    )))
}

fn package_managed_prefixes() -> &'static [&'static str] {
    &[
        "/opt/homebrew/",
        "/usr/local/Cellar/",
        "/usr/bin/",
        "/bin/",
        "/sbin/",
        "/usr/sbin/",
        "/nix/store/",
    ]
}

fn is_writable_dir(path: &Path) -> bool {
    let probe = path.join(".ai-memory-upgrade-write-probe");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn is_writable_file(path: &Path) -> bool {
    fs::OpenOptions::new().write(true).open(path).is_ok()
}

fn release_base_url(config: &Config) -> String {
    config
        .release_base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://github.com/{RELEASE_OWNER_REPO}/releases"))
}

fn release_asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("ai-memory-linux-x86_64.tar.gz"),
        ("linux", "aarch64") => Some("ai-memory-linux-aarch64.tar.gz"),
        ("macos", "aarch64") => Some("ai-memory-macos-aarch64.tar.gz"),
        ("macos", "x86_64") => Some("ai-memory-macos-x86_64.tar.gz"),
        _ => None,
    }
}

fn versions_match(tag: &str, current: &str) -> bool {
    normalize_version(tag) == normalize_version(current)
}

fn normalize_version(raw: &str) -> String {
    raw.trim().trim_start_matches('v').to_string()
}

async fn resolve_tag(fetcher: &ReqwestFetcher, base: &str, pinned: Option<&str>) -> Result<String> {
    if let Some(pinned) = pinned {
        return normalize_pinned_tag(pinned);
    }
    if base.contains("github.com") {
        return fetch_github_latest_tag(fetcher).await;
    }
    fetch_text_latest_tag(fetcher, base).await
}

fn normalize_pinned_tag(pinned: &str) -> Result<String> {
    let tag = pinned.trim();
    if tag.is_empty() {
        bail!("--version must be a non-empty release tag (e.g. v2.3.2)");
    }
    Ok(if tag.starts_with('v') {
        tag.to_string()
    } else {
        format!("v{tag}")
    })
}

async fn fetch_github_latest_tag(fetcher: &ReqwestFetcher) -> Result<String> {
    let api = format!("https://api.github.com/repos/{RELEASE_OWNER_REPO}/releases/latest");
    let body = fetcher
        .get_text(&api)
        .await
        .context("fetching latest release")?;
    let json: serde_json::Value =
        serde_json::from_str(&body).context("parsing latest release JSON")?;
    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .context("latest release JSON missing tag_name")?;
    Ok(tag.to_string())
}

async fn fetch_text_latest_tag(fetcher: &ReqwestFetcher, base: &str) -> Result<String> {
    // Prefer the GitHub API when talking to github.com; for a test base URL
    // fall back to a `{base}/latest/tag` text endpoint.
    let tag = fetcher
        .get_text(&format!("{base}/latest/tag"))
        .await
        .context("fetching latest tag")?
        .trim()
        .to_string();
    if tag.is_empty() {
        bail!("latest tag endpoint returned empty body");
    }
    Ok(tag)
}

fn parse_sha256_sidecar(text: &str, expected_filename: &str) -> Result<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let hash = parts.next().context("checksum line missing hash")?;
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("checksum line has invalid sha256 hash: {hash}");
        }
        if let Some(name) = parts.next() {
            let name = name.trim_start_matches('*');
            if name != expected_filename {
                bail!("checksum sidecar names {name}, expected {expected_filename}");
            }
        }
        return Ok(hash.to_ascii_lowercase());
    }
    bail!("checksum sidecar contained no hash line");
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn extract_release_archive(bytes: &[u8], dest: &Path) -> Result<()> {
    let decoder = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(false);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let entry_type = entry.header().entry_type();
        validate_release_entry(&path, entry_type)?;
        entry
            .unpack_in(dest)
            .with_context(|| format!("extracting {}", path.display()))?;
    }
    Ok(())
}

fn is_regular_file(entry_type: tar::EntryType) -> bool {
    entry_type.is_file() || entry_type == tar::EntryType::GNUSparse
}

fn validate_release_entry(path: &Path, entry_type: tar::EntryType) -> Result<()> {
    if !path.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("release archive contains unsafe path: {}", path.display());
    }
    if entry_type.is_symlink() || entry_type.is_hard_link() {
        bail!(
            "release archive contains unsupported link entry: {}",
            path.display()
        );
    }
    if !(is_regular_file(entry_type) || entry_type.is_dir()) {
        bail!(
            "release archive contains unsupported entry type: {}",
            path.display()
        );
    }
    if !is_allowed_release_path(path) {
        bail!(
            "release archive contains unexpected path: {}",
            path.display()
        );
    }
    Ok(())
}

fn is_allowed_release_path(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    path_str == BINARY_NAME
        || path_str == HOOKS_DIR_NAME
        || path_str
            .strip_prefix(HOOKS_DIR_NAME)
            .is_some_and(|rest| rest.starts_with('/'))
        || path_str == "README.md"
        || path_str == "LICENSE"
        || path_str.starts_with("docs/")
        || path_str.starts_with("crates/")
}

fn replace_file_atomic(src: &Path, dest: &Path) -> Result<()> {
    let tmp = dest.with_extension("new");
    if tmp.exists() {
        fs::remove_file(&tmp).with_context(|| format!("removing stale {}", tmp.display()))?;
    }
    fs::copy(src, &tmp).with_context(|| format!("copying to {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(UNIX_EXECUTABLE_MODE))
            .with_context(|| format!("chmod +x {}", tmp.display()))?;
    }
    fs::rename(&tmp, dest).with_context(|| {
        format!(
            "renaming {} -> {} (left {} in place on failure)",
            tmp.display(),
            dest.display(),
            tmp.display()
        )
    })?;
    Ok(())
}

fn replace_dir_atomic(src: &Path, dest: &Path) -> Result<()> {
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("destination {} has no parent", dest.display()))?;
    let tmp = prepare_dir_swap_staging(src, parent, dest)?;
    let backup = move_aside_for_swap(parent, dest)?;
    commit_dir_swap(&tmp, dest, &backup)
}

fn prepare_dir_swap_staging(src: &Path, parent: &Path, dest: &Path) -> Result<PathBuf> {
    let tmp = sibling_swap_path(parent, dest, "new");
    remove_dir_if_exists(&tmp)?;
    copy_dir_recursive(src, &tmp)?;
    Ok(tmp)
}

fn move_aside_for_swap(parent: &Path, dest: &Path) -> Result<PathBuf> {
    let backup = sibling_swap_path(parent, dest, "old");
    remove_dir_if_exists(&backup)?;
    if dest.exists() {
        fs::rename(dest, &backup)
            .with_context(|| format!("moving {} aside to {}", dest.display(), backup.display()))?;
    }
    Ok(backup)
}

fn commit_dir_swap(tmp: &Path, dest: &Path, backup: &Path) -> Result<()> {
    if let Err(err) = fs::rename(tmp, dest) {
        if backup.exists() {
            let _ = fs::rename(backup, dest);
        }
        return Err(err).with_context(|| format!("renaming {} -> {}", tmp.display(), dest.display()));
    }
    let _ = remove_dir_if_exists(backup);
    Ok(())
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    fs::remove_dir_all(path).with_context(|| format!("removing stale {}", path.display()))
}

fn sibling_swap_path(parent: &Path, dest: &Path, suffix: &str) -> PathBuf {
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(HOOKS_DIR_NAME);
    parent.join(format!(".{name}.{suffix}"))
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dest.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &to)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), &to)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// List agent dirs under a staged hooks root, skipping shared helper dirs.
///
/// `lib` and `_*-prefixed` directories hold shared helpers, not agents (#38).
/// Unknown directory names are omitted (callers may log them).
fn list_staged_agents(hooks_root: &Path) -> Result<(StagedAgentList, Vec<String>)> {
    let mut agents = StagedAgentList::new();
    let mut unknown = Vec::new();
    for entry in fs::read_dir(hooks_root)
        .with_context(|| format!("reading staged hooks at {}", hooks_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == "lib" || name.starts_with('_') {
            continue;
        }
        match AgentChoice::from_str(name, true) {
            Ok(agent) => agents.push((name.to_string(), agent)),
            Err(_) => unknown.push(name.to_string()),
        }
    }
    agents.sort_by(|a, b| a.0.cmp(&b.0));
    unknown.sort();
    Ok((agents, unknown))
}

fn refresh_staged_hooks(config: &Config) -> Result<()> {
    let hooks_root = staged_hooks_root(config);
    if !hooks_root.is_dir() {
        println!("→ no staged hook scripts found at {}", hooks_root.display());
        println!("  (nothing to refresh — install-hooks hasn't been run with --apply yet)");
        return Ok(());
    }
    let agents = collect_refreshable_agents(&hooks_root)?;
    if agents.is_empty() {
        println!(
            "→ no staged agent dirs found under {}",
            hooks_root.display()
        );
        return Ok(());
    }
    refresh_agent_list(config, &agents);
    Ok(())
}

fn staged_hooks_root(config: &Config) -> PathBuf {
    install_hooks::hook_staging_root(
        &config.data_dir,
        ai_memory_wiki::backup::running_in_container(),
    )
    .join(HOOKS_DIR_NAME)
}

fn collect_refreshable_agents(hooks_root: &Path) -> Result<StagedAgentList> {
    let (agents, unknown) = list_staged_agents(hooks_root)?;
    for name in &unknown {
        println!("  (skipping unknown staged agent dir `{name}`)");
    }
    Ok(agents)
}

fn refresh_agent_list(config: &Config, agents: &StagedAgentList) {
    let names: Vec<_> = agents.iter().map(|(n, _)| n.as_str()).collect();
    println!("→ refreshing staged hook scripts for: {}", names.join(" "));
    for (name, agent) in agents {
        apply_staged_agent_hooks(config, name, *agent);
    }
}

fn apply_staged_agent_hooks(config: &Config, name: &str, agent: AgentChoice) {
    println!("    ai-memory install-hooks --agent {name} --apply");
    if let Err(err) = install_hooks::run(config, apply_hooks_args(agent)) {
        println!(
            "      (skipped — {err:#}; re-run with the same --server-url / --auth-token used originally)"
        );
    }
}

fn apply_hooks_args(agent: AgentChoice) -> InstallHooksArgs {
    InstallHooksArgs {
        agent,
        hooks_dir: None,
        server_url: None,
        auth_token: None,
        as_user: None,
        apply: true,
        config_file: None,
        project_strategy: None,
        capture_assistant: false,
        capture_mode: None,
        no_capture_prompts: false,
        capture_prompts: false,
        profile: None,
    }
}

fn warn_remote_server(config: &Config) {
    if !config.server_url_configured() {
        return;
    }
    if is_loopback_url(&config.server_url) {
        return;
    }
    eprintln!(
        "note: AI_MEMORY_SERVER_URL={} looks remote — this upgrade refreshed only the local \
         binary and hooks; redeploy the remote server separately",
        config.server_url
    );
}

fn is_loopback_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("127.0.0.1")
        || lower.contains("localhost")
        || lower.contains("[::1]")
        || lower.contains("0.0.0.0")
}

struct ReqwestFetcher {
    client: reqwest::Client,
    max_bytes: usize,
}

impl ReqwestFetcher {
    fn new() -> Result<Self> {
        Self::with_max_bytes(MAX_RELEASE_DOWNLOAD_BYTES)
    }

    fn with_max_bytes(max_bytes: usize) -> Result<Self> {
        if max_bytes == 0 {
            bail!("upgrade download limit must be greater than zero");
        }
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .context("building HTTP client")?;
        Ok(Self { client, max_bytes })
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("GET {url} returned {status}");
        }
        refuse_oversized_content_length(url, response.content_length(), self.max_bytes)?;
        read_body_capped(&mut response, url, self.max_bytes).await
    }

    async fn get_text(&self, url: &str) -> Result<String> {
        let bytes = self.get_bytes(url).await?;
        String::from_utf8(bytes).context("response body is not UTF-8")
    }
}

fn refuse_oversized_content_length(url: &str, content_len: Option<u64>, max_bytes: usize) -> Result<()> {
    if let Some(len) = content_len
        && content_length_exceeds_limit(len, max_bytes)
    {
        bail!(
            "GET {url} Content-Length {len} exceeds upgrade download limit \
             of {max_bytes} bytes"
        );
    }
    Ok(())
}

async fn read_body_capped(
    response: &mut reqwest::Response,
    url: &str,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    // Stream with a hard cap — Content-Length can lie or be absent.
    let mut out = reserve_body_buffer(response.content_length(), max_bytes);
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("reading body from {url}"))?
    {
        append_chunk_within_limit(&mut out, &chunk, url, max_bytes)?;
    }
    Ok(out)
}

fn reserve_body_buffer(content_len: Option<u64>, max_bytes: usize) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(len) = content_len
        && let Ok(hint) = usize::try_from(len)
    {
        out.reserve(hint.min(max_bytes));
    }
    out
}

fn append_chunk_within_limit(
    out: &mut Vec<u8>,
    chunk: &[u8],
    url: &str,
    max_bytes: usize,
) -> Result<()> {
    let over_limit = out
        .len()
        .checked_add(chunk.len())
        .is_none_or(|next| next > max_bytes);
    if over_limit {
        bail!("GET {url} body exceeded upgrade download limit of {max_bytes} bytes");
    }
    out.extend_from_slice(chunk);
    Ok(())
}

/// `true` when `content_length` is strictly greater than `max_bytes`.
///
/// Compares in `u64` so a 32-bit `usize` limit never truncates via `as u64`
/// in the wrong direction. If `max_bytes` somehow cannot fit in `u64`
/// (theoretical >64-bit usize), every finite Content-Length is treated as
/// under the limit and the streamed `checked_add` gate still enforces it.
fn content_length_exceeds_limit(content_length: u64, max_bytes: usize) -> bool {
    match u64::try_from(max_bytes) {
        Ok(max) => content_length > max,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tar::Header;

    #[test]
    fn asset_name_matches_release_matrix_for_this_host() {
        // Compile-time host must map or explicitly fail — never invent a name.
        let name = release_asset_name();
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => assert_eq!(name, Some("ai-memory-linux-x86_64.tar.gz")),
            ("linux", "aarch64") => assert_eq!(name, Some("ai-memory-linux-aarch64.tar.gz")),
            ("macos", "aarch64") => assert_eq!(name, Some("ai-memory-macos-aarch64.tar.gz")),
            ("macos", "x86_64") => assert_eq!(name, Some("ai-memory-macos-x86_64.tar.gz")),
            ("windows", _) => assert_eq!(name, None),
            _ => {}
        }
    }

    #[test]
    fn parse_sha256_sidecar_accepts_sha256sum_format() -> Result<()> {
        let text = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  ai-memory-macos-aarch64.tar.gz\n";
        let hash = parse_sha256_sidecar(text, "ai-memory-macos-aarch64.tar.gz")?;
        assert_eq!(
            hash,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        Ok(())
    }

    #[test]
    fn parse_sha256_sidecar_rejects_wrong_filename() -> Result<()> {
        let text =
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  other.tar.gz\n";
        match parse_sha256_sidecar(text, "ai-memory-macos-aarch64.tar.gz") {
            Ok(_) => bail!("wrong filename must fail"),
            Err(err) => assert!(err.to_string().contains("expected")),
        }
        Ok(())
    }

    #[test]
    fn parse_sha256_sidecar_rejects_bad_hash() -> Result<()> {
        match parse_sha256_sidecar("not-a-hash  file.tar.gz\n", "file.tar.gz") {
            Ok(_) => bail!("bad hash must fail"),
            Err(err) => assert!(err.to_string().contains("invalid sha256")),
        }
        Ok(())
    }

    #[test]
    fn versions_match_strips_v_prefix() {
        assert!(versions_match("v2.3.2", "2.3.2"));
        assert!(versions_match("2.3.2", "v2.3.2"));
        assert!(!versions_match("v2.3.2", "2.3.1"));
    }

    #[test]
    fn classify_rejects_homebrew_prefix() -> Result<()> {
        let class = classify_install(Path::new("/opt/homebrew/bin/ai-memory"))?;
        assert!(matches!(class, InstallClass::Unsupported(_)));
        Ok(())
    }

    #[test]
    fn classify_rejects_usr_bin() -> Result<()> {
        let class = classify_install(Path::new("/usr/bin/ai-memory"))?;
        assert!(matches!(class, InstallClass::Unsupported(_)));
        Ok(())
    }

    #[test]
    fn classify_accepts_writable_user_prefix() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let exe = dir.path().join("ai-memory");
        fs::write(&exe, b"fake")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&exe, fs::Permissions::from_mode(0o755))?;
        }
        let class = classify_install(&exe)?;
        // In CI containers /.dockerenv may exist — then Unsupported is correct.
        if Path::new("/.dockerenv").exists() {
            assert!(matches!(class, InstallClass::Unsupported(_)));
        } else {
            assert_eq!(class, InstallClass::Supported);
        }
        Ok(())
    }

    #[test]
    fn validate_rejects_path_traversal() -> Result<()> {
        match validate_release_entry(Path::new("../evil"), tar::EntryType::Regular) {
            Ok(()) => bail!("path traversal must fail"),
            Err(err) => assert!(err.to_string().contains("unsafe"), "{err}"),
        }
        Ok(())
    }

    #[test]
    fn validate_rejects_unexpected_paths() -> Result<()> {
        match validate_release_entry(Path::new("etc/passwd"), tar::EntryType::Regular) {
            Ok(()) => bail!("unexpected path must fail"),
            Err(err) => assert!(err.to_string().contains("unexpected"), "{err}"),
        }
        Ok(())
    }

    #[test]
    fn extract_and_verify_round_trip() -> Result<()> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_path("ai-memory")?;
            header.set_size(11);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append(&header, &b"hello-world"[..])?;
            builder.finish()?;
        }
        let mut gz_bytes = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            let mut enc = GzEncoder::new(&mut gz_bytes, Compression::fast());
            std::io::Write::write_all(&mut enc, &tar_bytes)?;
            enc.finish()?;
        }
        let hash = sha256_hex(&gz_bytes);
        let sidecar = format!("{hash}  ai-memory-macos-aarch64.tar.gz\n");
        assert_eq!(
            parse_sha256_sidecar(&sidecar, "ai-memory-macos-aarch64.tar.gz")?,
            hash
        );
        let dest = tempfile::tempdir()?;
        extract_release_archive(&gz_bytes, dest.path())?;
        assert_eq!(fs::read(dest.path().join("ai-memory"))?, b"hello-world");
        Ok(())
    }

    #[test]
    fn replace_file_atomic_swaps_contents() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let dest = dir.path().join("ai-memory");
        let src = dir.path().join("fresh");
        fs::write(&dest, b"old")?;
        fs::write(&src, b"new")?;
        replace_file_atomic(&src, &dest)?;
        assert_eq!(fs::read(&dest)?, b"new");
        assert!(!dest.with_extension("new").exists());
        Ok(())
    }

    #[test]
    fn replace_dir_atomic_swaps_tree_and_cleans_backup() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let dest = dir.path().join("hooks");
        let src = dir.path().join("fresh-hooks");
        fs::create_dir_all(dest.join("claude-code"))?;
        fs::write(dest.join("claude-code/old.sh"), b"old")?;
        fs::create_dir_all(src.join("claude-code"))?;
        fs::write(src.join("claude-code/new.sh"), b"new")?;

        replace_dir_atomic(&src, &dest)?;

        assert_eq!(fs::read(dest.join("claude-code/new.sh"))?, b"new");
        assert!(!dest.join("claude-code/old.sh").exists());
        assert!(!dir.path().join(".hooks.old").exists());
        assert!(!dir.path().join(".hooks.new").exists());
        Ok(())
    }

    #[test]
    fn release_base_url_prefers_config_override() {
        let mut config = Config::default();
        assert!(
            release_base_url(&config).contains("github.com/akitaonrails/ai-memory/releases")
        );
        config.release_base_url = Some(" http://127.0.0.1:9/releases ".into());
        assert_eq!(release_base_url(&config), "http://127.0.0.1:9/releases");
    }

    #[tokio::test]
    async fn resolve_tag_reads_latest_tag_from_non_github_base() -> Result<()> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0_u8; 4096];
            let _ = stream.read(&mut buf).await;
            let body = "v9.9.9";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });

        let fetcher = ReqwestFetcher::new()?;
        let base = format!("http://{addr}/releases");
        let tag = resolve_tag(&fetcher, &base, None).await?;
        assert_eq!(tag, "v9.9.9");
        Ok(())
    }

    #[tokio::test]
    async fn resolve_tag_honors_pinned_version_without_network() -> Result<()> {
        let fetcher = ReqwestFetcher::new()?;
        let tag = resolve_tag(&fetcher, "http://127.0.0.1:1/unused", Some("2.3.2")).await?;
        assert_eq!(tag, "v2.3.2");
        Ok(())
    }

    #[tokio::test]
    async fn get_bytes_rejects_oversized_content_length() -> Result<()> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0_u8; 4096];
            let _ = stream.read(&mut buf).await;
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 999\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(response.as_bytes()).await;
        });

        let fetcher = ReqwestFetcher::with_max_bytes(64)?;
        match fetcher.get_bytes(&format!("http://{addr}/big")).await {
            Ok(_) => bail!("oversized Content-Length must fail"),
            Err(err) => assert!(
                err.to_string().contains("Content-Length"),
                "unexpected error: {err:#}"
            ),
        }
        Ok(())
    }

    #[tokio::test]
    async fn get_bytes_rejects_stream_past_limit_without_content_length() -> Result<()> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0_u8; 4096];
            let _ = stream.read(&mut buf).await;
            // No Content-Length; body is 80 bytes against a 64-byte cap.
            let body = vec![b'x'; 80];
            let header = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(&body).await;
        });

        let fetcher = ReqwestFetcher::with_max_bytes(64)?;
        match fetcher.get_bytes(&format!("http://{addr}/stream")).await {
            Ok(_) => bail!("stream past limit must fail"),
            Err(err) => assert!(
                err.to_string().contains("exceeded upgrade download limit"),
                "unexpected error: {err:#}"
            ),
        }
        Ok(())
    }

    #[test]
    fn with_max_bytes_rejects_zero() -> Result<()> {
        match ReqwestFetcher::with_max_bytes(0) {
            Ok(_) => bail!("zero cap must fail"),
            Err(err) => assert!(err.to_string().contains("greater than zero")),
        }
        Ok(())
    }

    #[test]
    fn content_length_limit_compare_is_strict() {
        assert!(!content_length_exceeds_limit(64, 64));
        assert!(content_length_exceeds_limit(65, 64));
        assert!(!content_length_exceeds_limit(0, 1));
    }

    #[test]
    fn is_loopback_detects_common_forms() {
        assert!(is_loopback_url("http://127.0.0.1:49374"));
        assert!(is_loopback_url("http://localhost:49374"));
        assert!(!is_loopback_url("http://192.168.1.10:49374"));
    }

    #[test]
    fn list_staged_agents_keeps_known_and_skips_helpers() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let hooks = dir.path();
        for name in ["claude-code", "cursor", "lib", "_shared", "not-an-agent"] {
            fs::create_dir(hooks.join(name))?;
        }
        fs::write(hooks.join("README.md"), b"ignore files")?;

        let (agents, unknown) = list_staged_agents(hooks)?;
        let names: Vec<_> = agents.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["claude-code", "cursor"]);
        assert_eq!(unknown, ["not-an-agent"]);
        assert!(agents.iter().all(|(_, a)| {
            matches!(a, AgentChoice::ClaudeCode | AgentChoice::Cursor)
        }));
        Ok(())
    }

    #[test]
    fn list_staged_agents_empty_root_returns_empty() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::create_dir(dir.path().join("lib"))?;
        let (agents, unknown) = list_staged_agents(dir.path())?;
        assert!(agents.is_empty());
        assert!(unknown.is_empty());
        Ok(())
    }
}
