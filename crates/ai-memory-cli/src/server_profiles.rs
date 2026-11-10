//! Named server profiles: per-repository routing of hook capture (#992).
//!
//! A machine can register several ai-memory servers under local names in
//! `<data_dir>/servers.toml`; a repository's `.ai-memory.toml` then selects one
//! with `server = "<name>"`. The marker is committed repository content — so
//! untrusted input — which is why it carries only a *name*: a cloned repository
//! can choose among servers this operator registered, never introduce a new
//! destination, and never see a credential.
//!
//! Every failure resolves to a [`Rejection`] and the caller drops the event.
//! Falling back to the install-time `--server-url` would deliver one team's
//! capture to another team's server, which is the exact outcome profiles exist
//! to prevent.
//!
//! Tokens live one per file under `<data_dir>/auth-tokens/<name>`, `0600`, and
//! never in `servers.toml`, a rendered hook config, or a process argv.
//!
//! Read on the hook hot path, so parsing uses `toml_edit` over this one file
//! and nothing else — no figment merge, no environment — for the same reason
//! `hook_spool::configured_server_url` does.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

/// `<data_dir>/servers.toml`.
const REGISTRY_FILE: &str = "servers.toml";
/// `<data_dir>/auth-tokens/` — one owner-only file per profile.
const TOKEN_DIR: &str = "auth-tokens";
/// A registry is a handful of short tables; anything larger is not one.
const MAX_REGISTRY_BYTES: u64 = 64 * 1024;
const MAX_NAME_CHARS: usize = 64;

/// A validated profile name: `[a-z0-9][a-z0-9_-]{0,63}`, and not a Windows
/// reserved device name.
///
/// Parsed once, because the name becomes a file name under `auth-tokens/`: a
/// marker naming `../auth-token` must never reach the filesystem as a path,
/// and `nul` or `con` would open a device instead of a file on Windows. The
/// device names are refused on every platform so a registry stays portable.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ProfileName(String);

impl ProfileName {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let mut chars = raw.chars();
        let first = chars.next()?;
        let valid = raw.len() <= MAX_NAME_CHARS
            && (first.is_ascii_lowercase() || first.is_ascii_digit())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
            && !ai_memory_core::is_dos_device_name(raw);
        valid.then(|| Self(raw.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProfileName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One registered server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Profile {
    /// Server URL, trailing slashes trimmed.
    pub(crate) url: String,
    /// Directories allowed to select this profile, as written (`~/` allowed).
    pub(crate) roots: Vec<String>,
}

/// The parsed `servers.toml`. An absent file is an empty registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Registry {
    pub(crate) profiles: BTreeMap<ProfileName, Profile>,
}

/// Why a marker's `server` selection was refused. Each one drops the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rejection {
    /// The marker's value is not a valid profile name.
    InvalidName,
    /// `servers.toml` is unreadable, oversized, or malformed.
    InvalidRegistry,
    /// No profile by that name is registered.
    UnknownProfile,
    /// The profile has no stored token.
    NoToken,
    /// Several profiles are registered and this one declares no `roots`, so
    /// any repository could select it.
    RootsRequired,
    /// The marker sits outside every one of the profile's `roots`.
    OutsideRoots,
    /// A marker on the walk exists but could not be read, so whether it
    /// selects a profile is unknown.
    UnreadableMarker,
}

impl Rejection {
    /// Stable, secret-free label for `hook --check-capture` and warnings.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::InvalidName => "rejected-invalid-profile-name",
            Self::InvalidRegistry => "rejected-invalid-registry",
            Self::UnknownProfile => "rejected-unknown-profile",
            Self::NoToken => "rejected-no-token",
            Self::RootsRequired => "rejected-roots-required",
            Self::OutsideRoots => "rejected-outside-roots",
            Self::UnreadableMarker => "rejected-unreadable-marker",
        }
    }
}

/// A selection that passed every check: where to deliver, and with what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedServer {
    pub(crate) name: ProfileName,
    pub(crate) url: String,
    pub(crate) token: String,
}

fn registry_path(data_dir: &Path) -> PathBuf {
    data_dir.join(REGISTRY_FILE)
}

fn token_path(data_dir: &Path, name: &ProfileName) -> PathBuf {
    data_dir.join(TOKEN_DIR).join(name.as_str())
}

/// The name under `auth-tokens/` that holds this install's own root bearer
/// once `init` or `auth enable` provisions one. It shares the profile layout
/// and parsing rules, so `server add`, `server remove` and `auth enable`
/// refuse to let a registered profile of the same name share the file.
pub(crate) const ROOT_TOKEN_NAME: &str = "default";

/// `[auth].bearer_token_file` as `init` and `auth enable` record it: relative,
/// so it resolves against whichever path the data dir is mounted at.
pub(crate) const ROOT_TOKEN_RELATIVE: &str = "auth-tokens/default";

/// `<data_dir>/auth-tokens/default`.
#[must_use]
pub(crate) fn root_token_path(data_dir: &Path) -> PathBuf {
    data_dir.join(TOKEN_DIR).join(ROOT_TOKEN_NAME)
}

/// Write the root bearer to `path`: the token directory `0700`, the file
/// `0600`, replaced atomically.
///
/// # Errors
/// Propagates directory creation and write failures.
pub(crate) fn store_root_token(path: &Path, token: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        crate::commands::path_util::create_private_dir(dir)
            .context("cannot create the token directory")?;
    }
    write_private(path, &format!("{token}\n"))
}

/// Whether `servers.toml` registers a profile named [`ROOT_TOKEN_NAME`].
///
/// # Errors
/// An unreadable or invalid registry, which cannot prove the name is free.
pub(crate) fn root_token_name_is_registered(data_dir: &Path) -> Result<bool> {
    let name = ProfileName(ROOT_TOKEN_NAME.to_owned());
    Ok(load(data_dir)?.profiles.contains_key(&name))
}

/// Whether a registered profile would read the same file as the root bearer
/// at `root_token`: a profile named `default` while the root bearer lives at
/// `auth-tokens/default`. Its server would then be handed this install's root
/// credential.
pub(crate) fn profile_shares_root_token(
    data_dir: &Path,
    name: &ProfileName,
    root_token: Option<&Path>,
) -> bool {
    root_token.is_some_and(|path| {
        crate::marker::absolute_normalized(path)
            == crate::marker::absolute_normalized(&token_path(data_dir, name))
    })
}

/// Load `servers.toml`. A missing file is an empty registry; anything else
/// that is not exactly the documented shape is an error, so a typo can only
/// ever make routing refuse, never make it guess.
pub(crate) fn load(data_dir: &Path) -> Result<Registry> {
    read_bounded(&registry_path(data_dir))?.map_or_else(
        || Ok(Registry::default()),
        |text| validate(&parse_doc(&text)?),
    )
}

fn read_bounded(path: &Path) -> Result<Option<String>> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {REGISTRY_FILE}")),
    };
    if meta.len() > MAX_REGISTRY_BYTES {
        bail!("{REGISTRY_FILE} exceeds {MAX_REGISTRY_BYTES} bytes");
    }
    std::fs::read_to_string(path)
        .map(Some)
        .with_context(|| format!("cannot read {REGISTRY_FILE}"))
}

fn parse_doc(text: &str) -> Result<toml_edit::DocumentMut> {
    text.parse()
        .with_context(|| format!("{REGISTRY_FILE} is not valid TOML"))
}

fn validate(doc: &toml_edit::DocumentMut) -> Result<Registry> {
    let mut registry = Registry::default();
    for (key, item) in doc.iter() {
        if key != "servers" {
            bail!("unknown top-level key `{key}` in {REGISTRY_FILE}");
        }
        let servers = item
            .as_table_like()
            .with_context(|| format!("`servers` in {REGISTRY_FILE} must be a table"))?;
        for (raw_name, entry) in servers.iter() {
            let name = ProfileName::parse(raw_name)
                .with_context(|| format!("invalid profile name `{raw_name}`"))?;
            let table = entry
                .as_table_like()
                .with_context(|| format!("profile `{name}` must be a table"))?;
            let profile = parse_profile(table).with_context(|| format!("profile `{name}`"))?;
            registry.profiles.insert(name, profile);
        }
    }
    Ok(registry)
}

fn parse_profile(table: &dyn toml_edit::TableLike) -> Result<Profile> {
    let mut url = None;
    let mut roots = Vec::new();
    for (key, value) in table.iter() {
        match key {
            "url" => {
                url = Some(validate_url(
                    value.as_str().context("`url` must be a string")?,
                )?);
            }
            "roots" => {
                for root in value.as_array().context("`roots` must be an array")? {
                    let root = root
                        .as_str()
                        .context("every `roots` entry must be a string")?;
                    validate_root(root)?;
                    roots.push(root.to_owned());
                }
            }
            other => bail!("unknown key `{other}`"),
        }
    }
    Ok(Profile {
        url: url.context("no `url`")?,
        roots,
    })
}

/// Accept only `http(s)://` URLs with a host, normalised without trailing
/// slashes — rejected here rather than on every hook request.
fn validate_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    // A URL may carry `user:password@` userinfo; keep it out of the error text,
    // which lands on stderr/stdout (GHSA-6pmr-q79m-87vp item 4).
    let shown = crate::commands::render_shared::mask_url_userinfo(raw);
    let parsed = reqwest::Url::parse(trimmed).with_context(|| format!("`{shown}` is not a URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none_or(str::is_empty) {
        bail!("`{shown}` is not an http:// or https:// URL with a host");
    }
    Ok(trimmed.to_owned())
}

/// A root must be absolute or `~/`-relative: a relative root would resolve
/// against whatever directory the hook happened to run in.
fn validate_root(raw: &str) -> Result<()> {
    if raw == "~" || raw.starts_with("~/") || Path::new(raw).is_absolute() {
        Ok(())
    } else {
        bail!("root `{raw}` must be absolute or start with `~/`")
    }
}

fn expand_root(raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    let path = if raw == "~" {
        home?.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home?.join(rest)
    } else {
        PathBuf::from(raw)
    };
    Some(crate::marker::absolute_normalized(&path))
}

/// Read a profile's stored token. Trailing newline trimmed; blank is `None`.
pub(crate) fn read_token(data_dir: &Path, name: &ProfileName) -> Option<String> {
    crate::config::read_trimmed_secret(&token_path(data_dir, name))
}

/// Resolve a marker's `server` selection.
///
/// `marker_dir` is the directory of the marker that declared the key, in the
/// same lexical namespace as the hook's cwd (see `marker::absolute_normalized`);
/// `roots` are compared against it component-wise, so `~/work/team-b` admits
/// `~/work/team-b/repo` but not `~/work/team-bb`. A Windows root folds ASCII
/// case, since the filesystem does and the cwd a harness reports can differ
/// in case from the spelling the root was registered with.
pub(crate) fn resolve(
    data_dir: &Path,
    raw_name: &str,
    marker_dir: &Path,
    home: Option<&Path>,
) -> Result<ResolvedServer, Rejection> {
    let name = ProfileName::parse(raw_name).ok_or(Rejection::InvalidName)?;
    let registry = load(data_dir).map_err(|_| Rejection::InvalidRegistry)?;
    let profile = registry
        .profiles
        .get(&name)
        .ok_or(Rejection::UnknownProfile)?;
    if profile.roots.is_empty() {
        if registry.profiles.len() > 1 {
            return Err(Rejection::RootsRequired);
        }
    } else {
        let marker_dir = crate::marker::absolute_normalized(marker_dir);
        let admitted = profile
            .roots
            .iter()
            .filter_map(|root| expand_root(root, home))
            .any(|root| crate::marker::path_contains(&root, &marker_dir));
        if !admitted {
            return Err(Rejection::OutsideRoots);
        }
    }
    with_token(data_dir, name, profile)
}

/// Which server a checkout's events and commands go to (#992, #2016).
#[derive(Debug)]
pub(crate) enum Route {
    /// No marker selects a server: the install's own server (the hook's
    /// install-time `--server-url`, a command's config), exactly as before
    /// profiles existed.
    InstallDefault,
    /// The marker's profile resolved; its URL and token replace the install's.
    Profile(ResolvedServer),
    /// The marker selects a profile that did not resolve. Nothing is sent.
    Rejected {
        /// The selected name, when it is a valid one (safe to print).
        name: Option<ProfileName>,
        reason: Rejection,
    },
}

impl Route {
    pub(crate) fn profile_name(&self) -> Option<&ProfileName> {
        match self {
            Self::InstallDefault => None,
            Self::Profile(resolved) => Some(&resolved.name),
            Self::Rejected { name, .. } => name.as_ref(),
        }
    }

    /// Label for `--check-capture`. Never a URL, path, or token.
    pub(crate) fn resolution(&self) -> &'static str {
        match self {
            Self::InstallDefault => "install-default",
            Self::Profile(_) => "marker",
            Self::Rejected { reason, .. } => reason.as_str(),
        }
    }
}

/// The route for work done in `cwd`: the nearest marker on the walk that
/// declares `server` decides, through [`resolve`]. Hooks route each event
/// with it and checkout-scoped commands their requests, so both reach the
/// same server for the same directory.
pub(crate) fn route_for_cwd(data_dir: &Path, cwd: &str, home: Option<&Path>) -> Route {
    let Some(selection) = crate::marker::find_server_selection(cwd, home) else {
        return Route::InstallDefault;
    };
    let Some(raw_name) = selection.name else {
        return Route::Rejected {
            name: None,
            reason: Rejection::UnreadableMarker,
        };
    };
    match resolve(data_dir, &raw_name, &selection.marker_dir, home) {
        Ok(resolved) => Route::Profile(resolved),
        Err(reason) => Route::Rejected {
            name: ProfileName::parse(&raw_name),
            reason,
        },
    }
}

/// A registered profile's URL and stored token, without the `roots` check.
///
/// For a caller acting on a selection the hook already vetted against the
/// marker that made it — the backfill the hook spawned.
pub(crate) fn lookup(data_dir: &Path, name: &ProfileName) -> Result<ResolvedServer, Rejection> {
    let registry = load(data_dir).map_err(|_| Rejection::InvalidRegistry)?;
    let profile = registry
        .profiles
        .get(name)
        .ok_or(Rejection::UnknownProfile)?;
    with_token(data_dir, name.clone(), profile)
}

fn with_token(
    data_dir: &Path,
    name: ProfileName,
    profile: &Profile,
) -> Result<ResolvedServer, Rejection> {
    let token = read_token(data_dir, &name).ok_or(Rejection::NoToken)?;
    Ok(ResolvedServer {
        name,
        url: profile.url.clone(),
        token,
    })
}

/// Register or replace a profile, keeping any hand-written comments and other
/// profiles in `servers.toml` intact, then store its token when one is given.
///
/// The file is validated before it is rewritten, so `add` refuses to build on
/// a registry the hook would already reject as a whole.
pub(crate) fn add(
    data_dir: &Path,
    name: &ProfileName,
    url: &str,
    roots: &[String],
    token: Option<&str>,
) -> Result<AddOutcome> {
    let url = validate_url(url)?;
    let roots_were_omitted = roots.is_empty();
    for root in roots {
        validate_root(root)?;
    }
    let path = registry_path(data_dir);
    let mut doc = parse_doc(&read_bounded(&path)?.unwrap_or_default())?;
    let existing = validate(&doc)?.profiles.remove(name);
    // Re-running `add` to rotate a token or fix a URL must not silently lift
    // the roots restriction: omitted roots keep the ones already registered.
    let roots = match &existing {
        Some(profile) if roots_were_omitted => profile.roots.clone(),
        _ => roots.to_vec(),
    };
    // A stored token belongs to the URL it was registered with. Once the URL
    // changes it is discarded *before* the new URL is written, so there is no
    // moment at which one server's credential is paired with another server.
    let token = token.map(str::trim).filter(|t| !t.is_empty());
    let url_changed = existing.as_ref().is_some_and(|profile| profile.url != url);
    let token_discarded = url_changed && discard_token(data_dir, name)? && token.is_none();
    let servers = doc
        .entry("servers")
        .or_insert_with(|| {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            toml_edit::Item::Table(table)
        })
        .as_table_like_mut()
        .with_context(|| format!("`servers` in {REGISTRY_FILE} must be a table"))?;
    let mut entry = toml_edit::Table::new();
    entry.insert("url", toml_edit::value(url));
    if !roots.is_empty() {
        let array: toml_edit::Array = roots.iter().map(String::as_str).collect();
        entry.insert("roots", toml_edit::value(array));
    }
    servers.insert(name.as_str(), toml_edit::Item::Table(entry));
    write_private(&path, &doc.to_string())?;
    if let Some(token) = token {
        crate::commands::path_util::create_private_dir(&data_dir.join(TOKEN_DIR))
            .context("cannot create the token directory")?;
        write_private(&token_path(data_dir, name), token)?;
    }
    Ok(AddOutcome {
        token_discarded,
        roots_kept: roots_were_omitted && !roots.is_empty(),
    })
}

/// What `add` did beyond writing what it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AddOutcome {
    /// The URL changed without a new token, so the old server's token was
    /// removed and the profile refuses events until a token is added.
    pub(crate) token_discarded: bool,
    /// No roots were given, so the profile kept its registered ones.
    pub(crate) roots_kept: bool,
}

/// Remove a profile and its token. Returns whether the profile existed.
pub(crate) fn remove(data_dir: &Path, name: &ProfileName) -> Result<bool> {
    let path = registry_path(data_dir);
    let mut existed = false;
    if let Some(text) = read_bounded(&path)? {
        let mut doc = parse_doc(&text)?;
        existed = doc
            .get_mut("servers")
            .and_then(toml_edit::Item::as_table_like_mut)
            .and_then(|servers| servers.remove(name.as_str()))
            .is_some();
        if existed {
            write_private(&path, &doc.to_string())?;
        }
    }
    Ok(discard_token(data_dir, name)? || existed)
}

/// Remove every stored profile token, keeping the registry itself and the
/// server's own root bearer.
///
/// `uninstall` removes the hooks, which are the only readers of these
/// tokens; leaving them on disk would strand live credentials the same way
/// the single hook token used to (#552). The registry holds no secrets and
/// survives, so a later reinstall lists each profile with `token: missing`
/// and refuses its events until a token is stored again.
///
/// `keep` is the file `[auth].bearer_token_file` names. That one is the
/// server's credential, not a hook's: removing it would leave a server that
/// refuses to start until `ai-memory auth enable` mints a new one.
///
/// # Errors
/// Propagates IO failures other than "not found".
pub(crate) fn clear_tokens(data_dir: &Path, keep: Option<&Path>) -> std::io::Result<()> {
    let dir = data_dir.join(TOKEN_DIR);
    let keep = keep.map(crate::marker::absolute_normalized);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut kept = false;
    for entry in entries {
        let path = entry?.path();
        if keep.as_ref() == Some(&crate::marker::absolute_normalized(&path)) {
            kept = true;
            continue;
        }
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match removed {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    if kept {
        return Ok(());
    }
    match std::fs::remove_dir(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Delete a profile's token file. Returns whether one was there.
fn discard_token(data_dir: &Path, name: &ProfileName) -> Result<bool> {
    match std::fs::remove_file(token_path(data_dir, name)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).context("cannot remove token"),
    }
}

/// Atomic replace through the canonical [`ai_memory_wiki::write_atomic`]. Its
/// temp file is created `0600`, so replacing an existing file never inherits
/// wider permissions from it.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    ai_memory_wiki::write_atomic(path, contents.as_bytes())
        .map(|_| ())
        .with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_token(data_dir: &Path, name: &ProfileName, token: &str) -> Result<()> {
        crate::commands::path_util::create_private_dir(&data_dir.join(TOKEN_DIR))?;
        write_private(&token_path(data_dir, name), token)
    }

    fn name(raw: &str) -> ProfileName {
        ProfileName::parse(raw).unwrap()
    }

    #[test]
    fn profile_names_cannot_become_paths() {
        for bad in [
            "",
            "../auth-token",
            "a/b",
            "Team",
            "-x",
            "a b",
            "a.b",
            &"a".repeat(65),
            "con",
            "nul",
            "aux",
            "prn",
            "com1",
            "lpt9",
        ] {
            assert!(
                ProfileName::parse(bad).is_none(),
                "{bad:?} must be rejected"
            );
        }
        for good in [
            "team-a", "b", "0", "team_b-2", "console", "com", "com10", "nul-1",
        ] {
            assert!(
                ProfileName::parse(good).is_some(),
                "{good:?} must be accepted"
            );
        }
    }

    #[test]
    fn a_missing_registry_is_empty_and_an_unknown_profile_is_refused() {
        let dd = tempfile::tempdir().unwrap();
        assert_eq!(load(dd.path()).unwrap(), Registry::default());
        assert_eq!(
            resolve(dd.path(), "team-a", dd.path(), None),
            Err(Rejection::UnknownProfile)
        );
    }

    #[test]
    fn a_malformed_registry_refuses_every_selection() {
        let dd = tempfile::tempdir().unwrap();
        for text in [
            "[servers.a]\nurl = \"https://a.example\"\ntoken = \"leak\"\n",
            "server_url = \"https://a.example\"\n",
            "[servers.a]\nurl = \"ftp://a.example\"\n",
            "[servers.a]\nroots = [\"~/x\"]\n",
            "[servers.a]\nurl = \"https://a.example\"\nroots = [\"relative\"]\n",
            "[servers.\"Bad\"]\nurl = \"https://a.example\"\n",
            "not toml [",
        ] {
            std::fs::write(dd.path().join(REGISTRY_FILE), text).unwrap();
            store_token(dd.path(), &name("a"), "tok").unwrap();
            assert_eq!(
                resolve(dd.path(), "a", dd.path(), None),
                Err(Rejection::InvalidRegistry),
                "{text}"
            );
        }
    }

    #[test]
    fn an_oversized_registry_is_refused() {
        let dd = tempfile::tempdir().unwrap();
        let mut text = String::from("[servers.a]\nurl = \"https://a.example\"\n");
        text.push_str(&"#".repeat(usize::try_from(MAX_REGISTRY_BYTES).unwrap()));
        std::fs::write(dd.path().join(REGISTRY_FILE), text).unwrap();
        assert!(load(dd.path()).is_err());
    }

    /// A platform-absolute path string under `base`.
    fn under(base: &Path, rel: &str) -> String {
        base.join(rel).to_string_lossy().into_owned()
    }

    #[test]
    fn a_single_profile_without_roots_resolves_with_its_own_token() {
        let dd = tempfile::tempdir().unwrap();
        add(
            dd.path(),
            &name("a"),
            "https://a.example/",
            &[],
            Some("tok-a"),
        )
        .unwrap();
        let resolved = resolve(dd.path(), "a", &dd.path().join("anywhere"), None).unwrap();
        assert_eq!(resolved.url, "https://a.example");
        assert_eq!(resolved.token, "tok-a");
    }

    #[test]
    fn a_profile_without_a_token_is_refused() {
        let dd = tempfile::tempdir().unwrap();
        add(dd.path(), &name("a"), "https://a.example", &[], None).unwrap();
        assert_eq!(
            resolve(dd.path(), "a", &dd.path().join("anywhere"), None),
            Err(Rejection::NoToken)
        );
    }

    #[test]
    fn roots_are_required_once_a_second_profile_exists() {
        let dd = tempfile::tempdir().unwrap();
        let work = dd.path().join("work");
        add(
            dd.path(),
            &name("a"),
            "https://a.example",
            &[],
            Some("tok-a"),
        )
        .unwrap();
        add(
            dd.path(),
            &name("b"),
            "https://b.example",
            &[under(&work, "b")],
            Some("tok-b"),
        )
        .unwrap();
        assert_eq!(
            resolve(dd.path(), "a", &work.join("a"), None),
            Err(Rejection::RootsRequired)
        );
        assert!(resolve(dd.path(), "b", &work.join("b").join("repo"), None).is_ok());
    }

    #[test]
    fn roots_match_whole_components_and_expand_home() {
        let dd = tempfile::tempdir().unwrap();
        let home = dd.path().join("home");
        add(
            dd.path(),
            &name("b"),
            "https://b.example",
            &["~/work/team-b".to_owned()],
            Some("tok-b"),
        )
        .unwrap();
        let team_b = home.join("work").join("team-b");
        assert!(resolve(dd.path(), "b", &team_b, Some(&home)).is_ok());
        assert!(resolve(dd.path(), "b", &team_b.join("x").join("y"), Some(&home)).is_ok());
        for outside in [
            home.join("work").join("team-bb"),
            home.join("work"),
            team_b.join("..").join("team-a"),
            dd.path().join("elsewhere").join("work").join("team-b"),
        ] {
            assert_eq!(
                resolve(dd.path(), "b", &outside, Some(&home)),
                Err(Rejection::OutsideRoots),
                "{}",
                outside.display()
            );
        }
        assert_eq!(
            resolve(dd.path(), "b", &team_b, None),
            Err(Rejection::OutsideRoots),
            "a `~` root cannot match when home is unknown"
        );
    }

    /// Case folding is scoped to Windows roots: on a case-sensitive
    /// filesystem `work/Team-B` and `work/team-b` are different directories
    /// and a root over one must not admit the other.
    #[cfg(unix)]
    #[test]
    fn a_posix_root_still_matches_case_sensitively() {
        let dd = tempfile::tempdir().unwrap();
        let home = dd.path().join("home");
        add(
            dd.path(),
            &name("b"),
            "https://b.example",
            &["~/work/Team-B".to_owned()],
            Some("tok-b"),
        )
        .unwrap();
        assert!(
            resolve(
                dd.path(),
                "b",
                &home.join("work").join("Team-B").join("repo"),
                Some(&home)
            )
            .is_ok()
        );
        assert_eq!(
            resolve(
                dd.path(),
                "b",
                &home.join("work").join("team-b").join("repo"),
                Some(&home)
            ),
            Err(Rejection::OutsideRoots)
        );
    }

    #[test]
    fn add_preserves_other_profiles_and_comments_and_remove_deletes_the_token() {
        let dd = tempfile::tempdir().unwrap();
        let root_a = toml_edit::Value::from(under(dd.path(), "a"));
        std::fs::write(
            dd.path().join(REGISTRY_FILE),
            format!(
                "# managed by hand\n[servers.a]\nurl = \"https://a.example\"\nroots = [{root_a}]\n"
            ),
        )
        .unwrap();
        add(
            dd.path(),
            &name("b"),
            "https://b.example",
            &[under(dd.path(), "b")],
            Some("tok-b"),
        )
        .unwrap();
        let text = std::fs::read_to_string(dd.path().join(REGISTRY_FILE)).unwrap();
        assert!(text.contains("# managed by hand"), "{text}");
        assert!(
            !text.contains("tok-b"),
            "tokens never go in the registry: {text}"
        );
        let registry = load(dd.path()).unwrap();
        assert_eq!(registry.profiles.len(), 2);

        assert!(remove(dd.path(), &name("b")).unwrap());
        assert!(read_token(dd.path(), &name("b")).is_none());
        assert_eq!(load(dd.path()).unwrap().profiles.len(), 1);
        assert!(!remove(dd.path(), &name("b")).unwrap());
    }

    #[test]
    fn add_to_an_inline_registry_preserves_the_other_profile() {
        let dd = tempfile::tempdir().unwrap();
        let root_a = under(dd.path(), "a");
        let root_b = under(dd.path(), "b");
        let encoded_root = toml_edit::Value::from(root_a.clone());
        std::fs::write(
            registry_path(dd.path()),
            format!(
                "# managed by hand\nservers = {{ a = {{ url = \"https://a.example\", roots = [{encoded_root}] }} }}\n"
            ),
        )
        .unwrap();
        store_token(dd.path(), &name("a"), "tok-a").unwrap();
        let before = load(dd.path()).unwrap();

        add(
            dd.path(),
            &name("b"),
            "https://b.example",
            &[root_b],
            Some("tok-b"),
        )
        .unwrap();

        let after = load(dd.path()).unwrap();
        assert_eq!(after.profiles.len(), 2);
        assert_eq!(after.profiles[&name("a")], before.profiles[&name("a")]);
        assert_eq!(read_token(dd.path(), &name("a")).as_deref(), Some("tok-a"));
        assert_eq!(read_token(dd.path(), &name("b")).as_deref(), Some("tok-b"));
        let text = std::fs::read_to_string(registry_path(dd.path())).unwrap();
        assert!(text.contains("# managed by hand"), "{text}");
        assert!(!text.contains("tok-a") && !text.contains("tok-b"), "{text}");
        assert!(remove(dd.path(), &name("b")).unwrap());
        assert_eq!(load(dd.path()).unwrap(), before);
        assert!(read_token(dd.path(), &name("b")).is_none());
    }

    #[test]
    fn inline_profile_updates_keep_roots_and_retire_only_replaced_url_tokens() {
        for changed_url in [false, true] {
            for replacement in [None, Some("tok-new")] {
                let dd = tempfile::tempdir().unwrap();
                let root = under(dd.path(), "a");
                let encoded_root = toml_edit::Value::from(root.clone());
                std::fs::write(
                    registry_path(dd.path()),
                    format!(
                        "servers = {{ a = {{ url = \"https://old.example\", roots = [{encoded_root}] }} }}\n"
                    ),
                )
                .unwrap();
                store_token(dd.path(), &name("a"), "tok-old").unwrap();
                assert_eq!(load(dd.path()).unwrap().profiles.len(), 1);
                let url = if changed_url {
                    "https://new.example"
                } else {
                    "https://old.example"
                };

                let outcome = add(dd.path(), &name("a"), url, &[], replacement).unwrap();

                let profile = &load(dd.path()).unwrap().profiles[&name("a")];
                assert_eq!(profile.url, url);
                assert_eq!(profile.roots, [root]);
                assert!(outcome.roots_kept);
                assert_eq!(
                    outcome.token_discarded,
                    changed_url && replacement.is_none()
                );
                let expected = replacement.or_else(|| (!changed_url).then_some("tok-old"));
                assert_eq!(read_token(dd.path(), &name("a")).as_deref(), expected);
            }
        }
    }

    /// Moving a profile to a new URL without a new token must not pair the
    /// old server's token with the new server.
    #[test]
    fn changing_the_url_without_a_token_discards_the_old_token() {
        let dd = tempfile::tempdir().unwrap();
        add(
            dd.path(),
            &name("b"),
            "https://old.example",
            &[],
            Some("tok-old"),
        )
        .unwrap();

        let outcome = add(dd.path(), &name("b"), "https://new.example", &[], None).unwrap();
        assert!(outcome.token_discarded);
        assert_eq!(read_token(dd.path(), &name("b")), None);
        assert_eq!(
            resolve(dd.path(), "b", dd.path(), None),
            Err(Rejection::NoToken)
        );

        let outcome = add(
            dd.path(),
            &name("b"),
            "https://new.example",
            &[],
            Some("tok-new"),
        )
        .unwrap();
        assert!(!outcome.token_discarded);
        assert_eq!(
            resolve(dd.path(), "b", dd.path(), None).unwrap().token,
            "tok-new"
        );

        let outcome = add(dd.path(), &name("b"), "https://new.example/", &[], None).unwrap();
        assert!(!outcome.token_discarded, "the same URL keeps its token");
        assert_eq!(
            read_token(dd.path(), &name("b")).as_deref(),
            Some("tok-new")
        );
    }

    /// Re-running `add` without `--root` (for a token rotation) keeps the
    /// registered roots instead of lifting the restriction.
    #[test]
    fn omitted_roots_keep_the_registered_ones() {
        let dd = tempfile::tempdir().unwrap();
        let root = under(dd.path(), "b");
        add(
            dd.path(),
            &name("b"),
            "https://b.example",
            std::slice::from_ref(&root),
            Some("t1"),
        )
        .unwrap();

        let outcome = add(dd.path(), &name("b"), "https://b.example", &[], Some("t2")).unwrap();
        assert!(outcome.roots_kept);
        assert_eq!(
            load(dd.path()).unwrap().profiles[&name("b")].roots,
            vec![root]
        );

        let other = under(dd.path(), "c");
        let outcome = add(
            dd.path(),
            &name("b"),
            "https://b.example",
            std::slice::from_ref(&other),
            None,
        )
        .unwrap();
        assert!(!outcome.roots_kept);
        assert_eq!(
            load(dd.path()).unwrap().profiles[&name("b")].roots,
            vec![other]
        );
    }

    /// Uninstall removes the tokens and nothing else; the registry stays so
    /// the profiles are listed as tokenless rather than forgotten.
    #[test]
    fn clear_tokens_removes_every_token_and_keeps_the_registry() {
        let dd = tempfile::tempdir().unwrap();
        add(
            dd.path(),
            &name("a"),
            "https://a.example",
            &[],
            Some("tok-a"),
        )
        .unwrap();
        add(
            dd.path(),
            &name("b"),
            "https://b.example",
            &[under(dd.path(), "b")],
            Some("tok-b"),
        )
        .unwrap();

        clear_tokens(dd.path(), None).unwrap();
        assert!(!dd.path().join(TOKEN_DIR).exists());
        assert_eq!(read_token(dd.path(), &name("a")), None);
        assert_eq!(read_token(dd.path(), &name("b")), None);
        assert_eq!(load(dd.path()).unwrap().profiles.len(), 2);
        assert_eq!(
            resolve(dd.path(), "b", &dd.path().join("b"), None),
            Err(Rejection::NoToken),
            "the profile is still registered, only its token is gone"
        );

        clear_tokens(dd.path(), None).expect("clearing an already-clear store is not an error");
    }

    /// The root bearer `[auth].bearer_token_file` names is the server's own
    /// credential: uninstall clears every profile token around it and keeps
    /// it, or the next `serve` would refuse to start.
    #[test]
    fn clear_tokens_keeps_the_root_bearer_and_removes_profile_tokens() {
        let dd = tempfile::tempdir().unwrap();
        let root = root_token_path(dd.path());
        store_root_token(&root, "root-secret").unwrap();
        add(
            dd.path(),
            &name("b"),
            "https://b.example",
            &[],
            Some("tok-b"),
        )
        .unwrap();

        clear_tokens(dd.path(), Some(&root)).unwrap();
        assert_eq!(
            read_token(dd.path(), &name("b")),
            None,
            "profile token removed"
        );
        assert_eq!(
            crate::config::read_trimmed_secret(&root).as_deref(),
            Some("root-secret"),
            "the root bearer survives"
        );

        // Control: without a configured root bearer the same file is just a
        // token in the directory, and goes with the rest.
        clear_tokens(dd.path(), None).unwrap();
        assert!(!dd.path().join(TOKEN_DIR).exists());
    }

    #[test]
    fn a_default_profile_shares_the_root_bearer_only_at_its_path() {
        let dd = tempfile::tempdir().unwrap();
        let root = root_token_path(dd.path());
        assert!(profile_shares_root_token(
            dd.path(),
            &name("default"),
            Some(&root)
        ));
        assert!(!profile_shares_root_token(
            dd.path(),
            &name("team-b"),
            Some(&root)
        ));
        assert!(!profile_shares_root_token(
            dd.path(),
            &name("default"),
            None
        ));
        assert!(!profile_shares_root_token(
            dd.path(),
            &name("default"),
            Some(&dd.path().join("elsewhere"))
        ));
    }

    #[test]
    fn add_refuses_to_build_on_an_invalid_registry() {
        let dd = tempfile::tempdir().unwrap();
        std::fs::write(dd.path().join(REGISTRY_FILE), "stray = 1\n").unwrap();
        assert!(add(dd.path(), &name("a"), "https://a.example", &[], None).is_err());
        assert_eq!(
            std::fs::read_to_string(dd.path().join(REGISTRY_FILE)).unwrap(),
            "stray = 1\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn tokens_are_owner_only_even_when_replacing_a_wider_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dd = tempfile::tempdir().unwrap();
        store_token(dd.path(), &name("a"), "old").unwrap();
        let path = token_path(dd.path(), &name("a"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        store_token(dd.path(), &name("a"), "new").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(dd.path().join(TOKEN_DIR))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(read_token(dd.path(), &name("a")).as_deref(), Some("new"));
    }
}
