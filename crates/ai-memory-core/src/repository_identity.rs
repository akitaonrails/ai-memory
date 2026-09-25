//! Repository identity: what names a repository independently of where it
//! sits on any one disk (#708).
//!
//! ## Why this exists
//!
//! A project is keyed by `(workspace_id, name)`, and an undeclared checkout
//! gets its name from its folder. Folder names are not unique: two unrelated
//! repositories both checked out as `api/` land in one project. On a
//! multi-user server with per-project grants that is an access problem, not
//! untidiness — whatever resolves a working directory to a project decides
//! which grant applies. And a path cannot fix it, because a path is
//! per-machine by construction: keying on one would split the same repository
//! into a different project on every device.
//!
//! So the client resolves an identity from what a repository carries with it,
//! first rung wins:
//!
//! 1. **explicit** — `identity = "…"` in `.ai-memory.toml`, written by a person;
//! 2. **manifest** — `project = "…"` in `.ai-memory.toml`, also written by a
//!    person;
//! 3. **git remote** — `upstream` when present, else `origin`, normalised;
//! 4. **folder name** — the basename.
//!
//! The two declarations rank above the remote because a statement beats an
//! inference. A fork that is its own product is the common case: its
//! `upstream` names the project it forked from, while its marker names what
//! it is. Letting the remote win would key the fork's memory under its
//! parent's identity.
//!
//! ## Which rungs route by identity
//!
//! Only [`IdentitySource::Explicit`] and [`IdentitySource::GitRemote`] — see
//! [`IdentitySource::routes_by_identity`]. A declared `project` already routes
//! by name, as it always has: the person chose that name, and two checkouts
//! declaring it share it by agreement. A folder name adds nothing a name
//! lookup does not already do. What is left is exactly the undeclared
//! checkout with a remote, which is the case that collided.
//!
//! ## Why the chain stops rather than searching
//!
//! When neither `upstream` nor `origin` exists, the chain falls to the next
//! rung instead of picking some other remote. Remote names are personal —
//! one person's `fork`, another's `mine` — so choosing among them would give
//! the same repository a different identity per person. `upstream` wins over
//! `origin` because in fork workflows every contributor's `origin` is their
//! own fork, which would give each of them a private memory.
//!
//! ## Why local-path remotes are rejected
//!
//! A remote can legitimately be a filesystem path — `/srv/git/api.git`,
//! `../sibling`, `file:///srv/git/api.git`, `C:\repos\api`. Those are paths,
//! with every problem paths have, so they yield no identity and the chain
//! moves on.
//!
//! ## Where this runs
//!
//! On the client, which is the only side that can see the checkout — a
//! remote server cannot read the user's disk. Normalising there also means a
//! remote with embedded credentials (`https://user:token@host/…`) never
//! leaves the machine. The native hook client calls this module; the shell,
//! PowerShell and TypeScript clients port [`normalize_remote_url`], and all
//! four are checked against `fixtures/remote_identity_cases.json`.

use serde::{Deserialize, Serialize};

/// The marker filename, matching `crates/ai-memory-cli/src/marker.rs`.
pub const MARKER_FILENAME: &str = ".ai-memory.toml";

/// Every marker name that is read, highest precedence first.
///
/// One entry today. The list stays because dropping a name that is still on
/// somebody's disk sends resolution down to the next rung — usually the
/// folder name — which silently re-homes that repository's memory. Any future
/// rename adds a name here rather than replacing one.
pub const MARKER_FILENAMES: &[&str] = &[MARKER_FILENAME];

/// Longest identity accepted from the wire. Real remotes are far shorter; the
/// bound exists so a client cannot park an arbitrary blob in a unique index.
pub const MAX_IDENTITY_LEN: usize = 512;

/// Which rung of the chain produced an identity.
///
/// Stored alongside the identity so an operator can tell a globally unique
/// identity from a merely local one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentitySource {
    /// `identity = "…"` in the marker. Outranks everything below it.
    ///
    /// Every rung beneath this one is either a name or an inference. This one
    /// is the answer to the cases neither can reach: a directory with no
    /// remote that must not collide with every other `notes/`, two checkouts
    /// that should share one memory, a monorepo subdirectory that deserves its
    /// own.
    Explicit,
    /// `project = "…"` in the marker. Unique by agreement.
    Manifest,
    /// A normalised `upstream` or `origin` URL. Globally unique.
    GitRemote,
    /// The directory's basename. **Not** globally unique.
    FolderName,
}

impl IdentitySource {
    /// The stored and wire spelling. Kept explicit rather than derived from the
    /// variant name so a rename in Rust cannot silently rewrite what is
    /// already in the database.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Manifest => "manifest",
            Self::GitRemote => "git_remote",
            Self::FolderName => "folder_name",
        }
    }

    /// Parse the stored spelling back. Unknown values are `None` rather than a
    /// default, so a row written by a newer version is visibly unreadable
    /// instead of quietly mis-typed.
    #[must_use]
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "explicit" => Some(Self::Explicit),
            "manifest" => Some(Self::Manifest),
            "git_remote" => Some(Self::GitRemote),
            "folder_name" => Some(Self::FolderName),
            _ => None,
        }
    }

    /// Whether an identity from this rung is unique beyond the local machine.
    #[must_use]
    pub fn is_globally_unique(self) -> bool {
        // An explicit declaration is as unique as the person writing it meant
        // it to be — which is the point of writing one.
        matches!(self, Self::Explicit | Self::GitRemote)
    }

    /// Whether the server routes a capture by this identity rather than by
    /// project name. See the module docs: a declared `project` and a folder
    /// name keep name routing, so only the two rungs that carry information a
    /// name does not are routed by identity.
    #[must_use]
    pub fn routes_by_identity(self) -> bool {
        matches!(self, Self::Explicit | Self::GitRemote)
    }
}

/// A resolved repository identity and the rung it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryIdentity {
    /// The identity itself, e.g. `github.com/acme/api`.
    pub identity: String,
    /// Which rung produced it.
    pub source: IdentitySource,
}

/// Everything the chain needs, gathered by the caller.
///
/// Taking these as data rather than reading git and the filesystem here keeps
/// this module pure: the whole chain is testable without a repository on disk,
/// and the crate stays free of a git dependency.
#[derive(Debug, Default, Clone)]
pub struct IdentityInputs<'a> {
    /// `identity = "…"` from the nearest `.ai-memory.toml`.
    pub explicit_identity: Option<&'a str>,
    /// `project = "…"` from the nearest `.ai-memory.toml`.
    pub manifest_name: Option<&'a str>,
    /// URL of the `upstream` remote, if any.
    pub upstream_remote: Option<&'a str>,
    /// URL of the `origin` remote, if any.
    pub origin_remote: Option<&'a str>,
    /// Basename of the repository root (or of the cwd when there is no repo).
    pub folder_name: Option<&'a str>,
}

/// Walk the chain and return the first rung that yields an identity.
///
/// Returns `None` only when every rung is empty.
#[must_use]
pub fn resolve(inputs: &IdentityInputs<'_>) -> Option<RepositoryIdentity> {
    // Rungs 1 and 2 are statements. Nothing inferred below overrides either —
    // including a git remote, because the reasons to write one down are
    // exactly the cases where the remote gives the wrong answer.
    if let Some(declared) = inputs.explicit_identity.and_then(non_empty) {
        return Some(RepositoryIdentity {
            identity: declared.to_lowercase(),
            source: IdentitySource::Explicit,
        });
    }
    if let Some(name) = inputs.manifest_name.and_then(non_empty) {
        return Some(RepositoryIdentity {
            identity: name.to_lowercase(),
            source: IdentitySource::Manifest,
        });
    }

    // Rung 3. `upstream` first. A remote that normalises to nothing — a local
    // path, or a string git accepted but we cannot key on — does not stop the
    // chain; it simply yields nothing and we continue.
    for remote in [inputs.upstream_remote, inputs.origin_remote]
        .into_iter()
        .flatten()
    {
        if let Some(identity) = normalize_remote_url(remote) {
            return Some(RepositoryIdentity {
                identity,
                source: IdentitySource::GitRemote,
            });
        }
    }

    // Rung 4. The folder name. Not globally unique; see `IdentitySource`.
    if let Some(name) = inputs.folder_name.and_then(non_empty) {
        return Some(RepositoryIdentity {
            identity: name.to_lowercase(),
            source: IdentitySource::FolderName,
        });
    }

    None
}

/// Accept an identity a client sent over the wire, or `None` to ignore it.
///
/// The server cannot re-run the chain — it never sees the checkout — so it
/// checks that what arrived has the shape the chain produces: case-folded,
/// trimmed, bounded, free of control characters, and for a remote, a
/// `host/path` with no empty segment. Anything else is dropped and the capture
/// routes by name, exactly as a client that sent nothing would. Only the rungs
/// that route by identity are accepted; the others carry nothing the server
/// uses.
#[must_use]
pub fn accept_wire_identity(identity: &str, source: &str) -> Option<RepositoryIdentity> {
    let source = IdentitySource::from_str_opt(source.trim())?;
    if !source.routes_by_identity() {
        return None;
    }
    let identity = identity.trim();
    if identity.is_empty()
        || identity.len() > MAX_IDENTITY_LEN
        || identity.chars().any(char::is_control)
        || identity != identity.to_lowercase()
    {
        return None;
    }
    if source == IdentitySource::GitRemote
        && (!identity.contains('/')
            || identity.split('/').any(str::is_empty)
            || identity.chars().any(char::is_whitespace))
    {
        return None;
    }
    Some(RepositoryIdentity {
        identity: identity.to_owned(),
        source,
    })
}

/// The project name to give a repository whose folder name is already held by
/// a different identity, before any numeric suffix.
///
/// For a path-shaped identity it is the last two segments joined with `-`
/// (`github.com/orgb/api` → `orgb-api`): the owner is what tells two `api`s
/// apart in a listing. Characters a project name cannot carry — `/` appears in
/// URL paths — become `-`.
#[must_use]
pub fn split_name_base(identity: &str) -> String {
    let segments: Vec<&str> = identity.split('/').filter(|s| !s.is_empty()).collect();
    let tail = if segments.len() >= 2 {
        segments[segments.len() - 2..].join("-")
    } else {
        segments.join("-")
    };
    let mut out = String::with_capacity(tail.len());
    for c in tail.chars() {
        let keep = c.is_alphanumeric() || matches!(c, '-' | '_' | '.');
        let c = if keep { c } else { '-' };
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    let out = out.trim_matches('-').to_owned();
    if out.is_empty() {
        "repository".to_owned()
    } else {
        out
    }
}

fn non_empty(s: &str) -> Option<&str> {
    let t = s.trim();
    (!t.is_empty()).then_some(t)
}

/// Normalise a git remote URL to a stable identity, or `None` if the URL does
/// not name a network-reachable repository.
///
/// All of these produce `github.com/acme/api`:
///
/// ```text
/// git@github.com:Acme/API.git
/// https://github.com/acme/api/
/// https://user@github.com:443/Acme/API
/// ssh://git@github.com:22/Acme/API.git
/// git://github.com/acme/api
/// ```
///
/// These produce `None`, because they are filesystem paths (hard rule #4):
///
/// ```text
/// /srv/git/api.git
/// ../sibling
/// file:///srv/git/api.git
/// C:\repos\api
/// ```
#[must_use]
pub fn normalize_remote_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    // Split off a scheme if there is one. Whether a scheme was present decides
    // how `:` is read further down, which is the subtle part of this function.
    let (scheme, rest) = match raw.find("://") {
        Some(idx) => {
            let scheme = raw[..idx].to_ascii_lowercase();
            (Some(scheme), &raw[idx + 3..])
        }
        None => (None, raw),
    };

    // `file://` names a path however it is dressed up.
    if scheme.as_deref() == Some("file") {
        return None;
    }

    // Credentials: strip up to the last `@` that precedes the path. Splitting
    // on the last one rather than the first keeps passwords containing `@`
    // from leaking a fragment of themselves into the identity.
    let host_and_path = {
        let path_start = rest.find('/').unwrap_or(rest.len());
        match rest[..path_start].rfind('@') {
            Some(at) => &rest[at + 1..],
            None => rest,
        }
    };

    let normalized = if scheme.is_some() {
        // URL form: `host[:port]/path`.
        let (host, path) = split_once_or_all(host_and_path, '/');
        let host = strip_port(host);
        if host.is_empty() || path.is_empty() {
            return None;
        }
        format!("{host}/{path}")
    } else {
        // No scheme. Either scp-like `host:path`, or a filesystem path.
        //
        // `:` before any `/` is what distinguishes them. `/srv/git/api.git`
        // has no `:` at all; `../sibling` likewise. A Windows drive letter
        // (`C:\repos\api`) does have one, hence the single-character guard —
        // no real hostname is one character long.
        let colon = host_and_path.find(':')?;
        let slash = host_and_path.find('/');
        if slash.is_some_and(|s| s < colon) {
            return None;
        }
        let (host, path) = host_and_path.split_at(colon);
        let path = &path[1..];
        if host.len() <= 1 || host.is_empty() || path.is_empty() {
            return None;
        }
        // A scp-like path is never absolute in practice, and a backslash means
        // we are looking at Windows rather than a repository path.
        if path.starts_with('\\') || path.contains('\\') {
            return None;
        }
        format!("{host}/{path}")
    };

    let identity = tidy(&normalized);

    // An identity with no `/` is a bare hostname, not a repository.
    if identity.is_empty() || !identity.contains('/') {
        return None;
    }
    Some(identity)
}

/// Strip a `:port` suffix from a host. Only ever called on the URL form, where
/// `:` unambiguously introduces a port — in scp-like syntax the same character
/// separates host from path.
fn strip_port(host: &str) -> &str {
    match host.rfind(':') {
        Some(idx) if host[idx + 1..].chars().all(|c| c.is_ascii_digit()) => &host[..idx],
        _ => host,
    }
}

fn split_once_or_all(s: &str, sep: char) -> (&str, &str) {
    match s.split_once(sep) {
        Some((a, b)) => (a, b),
        None => (s, ""),
    }
}

/// Case-fold, drop a trailing `.git`, and collapse the slash noise that
/// distinguishes otherwise identical URLs.
fn tidy(s: &str) -> String {
    let mut out = s.to_lowercase();
    while out.ends_with('/') {
        out.pop();
    }
    if let Some(stripped) = out.strip_suffix(".git") {
        out = stripped.to_string();
    }
    while out.ends_with('/') {
        out.pop();
    }
    // `https://host//acme//api` and `https://host/acme/api` are the same
    // repository as far as any server is concerned.
    while out.contains("//") {
        out = out.replace("//", "/");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property that defines the whole task: every spelling of the same
    /// repository has to land on one identity, or memory fragments per person.
    #[test]
    fn every_spelling_of_one_repository_agrees() {
        let expected = "github.com/acme/api";
        for url in [
            "git@github.com:Acme/API.git",
            "git@github.com:acme/api",
            "https://github.com/acme/api",
            "https://github.com/acme/api/",
            "https://github.com/acme/api.git",
            "https://github.com/Acme/API.git/",
            "https://user@github.com/acme/api",
            "https://user:secret@github.com/acme/api",
            "https://user@github.com:443/Acme/API",
            "ssh://git@github.com:22/Acme/API.git",
            "ssh://git@github.com/acme/api.git",
            "git://github.com/acme/api",
            "  https://github.com/acme/api  ",
            "https://github.com//acme//api",
        ] {
            assert_eq!(
                normalize_remote_url(url).as_deref(),
                Some(expected),
                "normalising {url}"
            );
        }
    }

    /// Hard rule #4. These are all paths, however git dresses them up, and a
    /// path must never become an identity.
    #[test]
    fn filesystem_paths_yield_no_identity() {
        for url in [
            "/srv/git/api.git",
            "../sibling",
            "./api",
            "file:///srv/git/api.git",
            "file://localhost/srv/git/api.git",
            r"C:\repos\api",
            r"c:\repos\api",
            "~/repos/api",
            "",
            "   ",
        ] {
            assert_eq!(normalize_remote_url(url), None, "rejecting {url}");
        }
    }

    #[test]
    fn a_bare_host_is_not_a_repository() {
        assert_eq!(normalize_remote_url("https://github.com"), None);
        assert_eq!(normalize_remote_url("https://github.com/"), None);
    }

    #[test]
    fn self_hosted_forges_and_deep_paths_survive() {
        assert_eq!(
            normalize_remote_url("git@git.internal.acme.dev:platform/tools/api.git").as_deref(),
            Some("git.internal.acme.dev/platform/tools/api")
        );
        assert_eq!(
            normalize_remote_url("https://gitlab.com/acme/group/subgroup/api.git").as_deref(),
            Some("gitlab.com/acme/group/subgroup/api")
        );
        assert_eq!(
            normalize_remote_url("ssh://git@ssh.dev.azure.com:22/v3/acme/proj/api").as_deref(),
            Some("ssh.dev.azure.com/v3/acme/proj/api")
        );
    }

    /// A repository whose name genuinely ends in `.git` would be mangled, but
    /// the suffix is stripped only once, so `api.git.git` keeps one.
    #[test]
    fn dot_git_is_stripped_once() {
        assert_eq!(
            normalize_remote_url("https://github.com/acme/api.git.git").as_deref(),
            Some("github.com/acme/api.git")
        );
    }

    #[test]
    fn upstream_wins_over_origin() {
        let got = resolve(&IdentityInputs {
            upstream_remote: Some("git@github.com:acme/api.git"),
            origin_remote: Some("git@github.com:contributor/api-fork.git"),
            ..Default::default()
        })
        .expect("an identity");
        assert_eq!(got.identity, "github.com/acme/api");
        assert_eq!(got.source, IdentitySource::GitRemote);
    }

    #[test]
    fn origin_is_used_when_there_is_no_upstream() {
        let got = resolve(&IdentityInputs {
            origin_remote: Some("https://github.com/acme/api"),
            ..Default::default()
        })
        .expect("an identity");
        assert_eq!(got.identity, "github.com/acme/api");
    }

    /// An unusable remote does not halt the chain, and no *other* remote is
    /// substituted — the caller never offers one.
    #[test]
    fn a_path_remote_falls_through_to_the_folder() {
        let got = resolve(&IdentityInputs {
            origin_remote: Some("/srv/git/api.git"),
            folder_name: Some("whatever"),
            ..Default::default()
        })
        .expect("an identity");
        assert_eq!(got.identity, "whatever");
        assert_eq!(got.source, IdentitySource::FolderName);
    }

    /// A declared project is a statement, and a statement beats the remote.
    /// This is the fork that is its own product: its `upstream` names what it
    /// forked from, its marker names what it is.
    #[test]
    fn a_declared_project_beats_the_git_remote() {
        let got = resolve(&IdentityInputs {
            manifest_name: Some("Lore"),
            upstream_remote: Some("https://github.com/upstream-owner/tool"),
            origin_remote: Some("git@github.com:fork-owner/lore.git"),
            folder_name: Some("my-checkout"),
            ..Default::default()
        })
        .expect("an identity");
        assert_eq!(got.identity, "lore");
        assert_eq!(got.source, IdentitySource::Manifest);
        assert!(
            !got.source.routes_by_identity(),
            "a declared name routes by name"
        );
    }

    #[test]
    fn the_chain_runs_in_order() {
        let remote_over_folder = resolve(&IdentityInputs {
            origin_remote: Some("https://github.com/acme/api"),
            folder_name: Some("on-disk"),
            ..Default::default()
        })
        .expect("an identity");
        assert_eq!(remote_over_folder.source, IdentitySource::GitRemote);

        let manifest_only = resolve(&IdentityInputs {
            manifest_name: Some("Declared"),
            folder_name: Some("on-disk"),
            ..Default::default()
        })
        .expect("an identity");
        assert_eq!(manifest_only.identity, "declared");
        assert_eq!(manifest_only.source, IdentitySource::Manifest);

        let folder_only = resolve(&IdentityInputs {
            folder_name: Some("On-Disk"),
            ..Default::default()
        })
        .expect("an identity");
        assert_eq!(folder_only.identity, "on-disk");
        assert_eq!(folder_only.source, IdentitySource::FolderName);

        assert!(resolve(&IdentityInputs::default()).is_none());
    }

    /// Blank strings are not declarations. A marker with `project = ""` should
    /// behave as if the field were absent rather than pin every repository to
    /// one empty identity.
    #[test]
    fn blank_rungs_are_skipped() {
        let got = resolve(&IdentityInputs {
            origin_remote: Some("   "),
            manifest_name: Some(""),
            folder_name: Some("api"),
            ..Default::default()
        })
        .expect("an identity");
        assert_eq!(got.identity, "api");
        assert_eq!(got.source, IdentitySource::FolderName);
    }

    /// The two clones from the task description: same repository, different
    /// directories, one identity. This is the regression that matters.
    #[test]
    fn two_clones_in_different_directories_share_an_identity() {
        let mac = resolve(&IdentityInputs {
            origin_remote: Some("git@github.com:acme/api.git"),
            folder_name: Some("api"),
            ..Default::default()
        })
        .expect("an identity");
        let linux = resolve(&IdentityInputs {
            origin_remote: Some("https://github.com/Acme/API"),
            folder_name: Some("acme-api"),
            ..Default::default()
        })
        .expect("an identity");
        assert_eq!(mac.identity, linux.identity);
    }

    /// The point of a declaration is to override what would otherwise be
    /// inferred. One that lost to a git remote would be useless in exactly the
    /// case people reach for it: two checkouts that should share one memory.
    #[test]
    fn an_explicit_declaration_beats_every_inferred_rung() {
        let got = resolve(&IdentityInputs {
            explicit_identity: Some("Acme Platform"),
            upstream_remote: Some("git@github.com:acme/api.git"),
            origin_remote: Some("git@github.com:contributor/api.git"),
            manifest_name: Some("declared"),
            folder_name: Some("on-disk"),
        })
        .expect("an identity");
        assert_eq!(got.identity, "acme platform");
        assert_eq!(got.source, IdentitySource::Explicit);
    }

    /// A folder with no git at all is the case the feature exists for: the
    /// chain would otherwise land on a basename that collides with every other
    /// `notes` folder in the world.
    #[test]
    fn a_declaration_rescues_a_folder_with_no_repository() {
        let inferred = resolve(&IdentityInputs {
            folder_name: Some("notes"),
            ..Default::default()
        })
        .expect("an identity");
        assert_eq!(inferred.source, IdentitySource::FolderName);
        assert!(!inferred.source.is_globally_unique());

        let declared = resolve(&IdentityInputs {
            explicit_identity: Some("ana/personal-notes"),
            folder_name: Some("notes"),
            ..Default::default()
        })
        .expect("an identity");
        assert_eq!(declared.identity, "ana/personal-notes");
        assert!(
            declared.source.is_globally_unique(),
            "having answered the question, the operator should stop being asked it"
        );
    }

    /// Two directories pointed at the same declared identity share a project —
    /// which is what "link a folder to a project" means.
    #[test]
    fn two_folders_can_be_linked_to_one_identity() {
        let a = resolve(&IdentityInputs {
            explicit_identity: Some("acme/platform"),
            folder_name: Some("frontend"),
            ..Default::default()
        })
        .unwrap();
        let b = resolve(&IdentityInputs {
            explicit_identity: Some("acme/platform"),
            origin_remote: Some("git@github.com:acme/backend.git"),
            folder_name: Some("backend"),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(a.identity, b.identity);
    }

    /// A blank declaration is not a declaration. An empty `identity = ""` must
    /// fall through rather than pin every folder to one empty identity.
    #[test]
    fn a_blank_declaration_falls_through() {
        let got = resolve(&IdentityInputs {
            explicit_identity: Some("   "),
            origin_remote: Some("https://github.com/acme/api"),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(got.source, IdentitySource::GitRemote);
    }

    #[test]
    fn source_round_trips_through_its_stored_spelling() {
        for source in [
            IdentitySource::Explicit,
            IdentitySource::GitRemote,
            IdentitySource::Manifest,
            IdentitySource::FolderName,
        ] {
            assert_eq!(IdentitySource::from_str_opt(source.as_str()), Some(source));
        }
        assert_eq!(IdentitySource::from_str_opt("something_new"), None);
        assert!(IdentitySource::GitRemote.is_globally_unique());
        assert!(!IdentitySource::Manifest.is_globally_unique());
        assert!(!IdentitySource::FolderName.is_globally_unique());
        assert!(IdentitySource::Explicit.routes_by_identity());
        assert!(IdentitySource::GitRemote.routes_by_identity());
        assert!(!IdentitySource::Manifest.routes_by_identity());
        assert!(!IdentitySource::FolderName.routes_by_identity());
    }

    const CASES: &str = include_str!("../fixtures/remote_identity_cases.json");

    /// The fixture every client normaliser is checked against. The Rust core
    /// is the reference: a case that fails here is a wrong fixture, a case
    /// that fails in a script client is a drifted port.
    #[test]
    fn the_shared_fixture_holds_for_the_reference_normaliser() {
        let cases: serde_json::Value = serde_json::from_str(CASES).unwrap();
        let normalize = cases["normalize"].as_array().unwrap();
        assert!(normalize.len() >= 20);
        for case in normalize {
            let url = case["url"].as_str().unwrap();
            let expected = case["identity"].as_str();
            assert_eq!(
                normalize_remote_url(url).as_deref(),
                expected,
                "normalising {url:?}"
            );
        }
        for case in cases["split_name"].as_array().unwrap() {
            let identity = case["identity"].as_str().unwrap();
            let expected = case["name"].as_str().unwrap();
            assert_eq!(
                split_name_base(identity),
                expected,
                "splitting {identity:?}"
            );
        }
    }

    /// What a client produces, the server accepts; what it cannot produce, or
    /// what does not route, is ignored rather than trusted.
    #[test]
    fn the_server_accepts_only_what_the_chain_can_produce() {
        let ok = accept_wire_identity("github.com/acme/api", "git_remote").unwrap();
        assert_eq!(ok.source, IdentitySource::GitRemote);
        assert!(accept_wire_identity("acme platform", "explicit").is_some());

        for (identity, source) in [
            ("github.com/acme/api", "manifest"),
            ("github.com/acme/api", "folder_name"),
            ("github.com/acme/api", "something_new"),
            ("GitHub.com/Acme/API", "git_remote"),
            ("github.com", "git_remote"),
            ("github.com//api", "git_remote"),
            ("github.com/acme/api/", "git_remote"),
            ("github.com/acme api", "git_remote"),
            ("", "explicit"),
            ("   ", "explicit"),
            ("line\nbreak", "explicit"),
        ] {
            assert!(
                accept_wire_identity(identity, source).is_none(),
                "accepted {identity:?} as {source}"
            );
        }
        assert!(accept_wire_identity(&"a/".repeat(MAX_IDENTITY_LEN), "explicit").is_none());
    }
}
