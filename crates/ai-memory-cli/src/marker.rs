//! `.ai-memory.toml` marker discovery and the scope it declares.
//!
//! One reader shared by both entry points that care about a repository's
//! declared identity:
//!
//! - the native hook path (`commands::hook_capture`), which forwards the
//!   marker's fields to the server as query params, and
//! - the thin-client CLI commands, which resolve `(workspace, project)`
//!   locally before calling `/admin/*` (see `commands::resolve_scope`).
//!
//! Before this module existed only the hook path read the marker, so a
//! repository could declare `workspace = "x"` and still have `run`,
//! `bootstrap`, `search` and friends silently resolve into `default` —
//! splitting one checkout across two scopes.
//!
//! Parsing is deliberately line-based (`parse_toml_key` / `parse_toml_flag`)
//! and mirrors `hooks/_lib.sh`, so the native binary and the POSIX shell
//! hooks agree on what a marker means. `[capture]` is the one section parsed
//! strictly, with a real TOML parser, and stays in `hook_capture`.

use std::path::{Path, PathBuf};

use crate::commands::path_util::home_dir;
use crate::config::RuntimeEnv;
use ai_memory_core::repository_identity::{
    IdentitySource, IdentityStyle, MARKER_FILENAME, MarkerAliases, RepositoryIdentity,
    accept_wire_identity,
};

const MAX_HOME_ROUTES: usize = 64;
const MAX_ROUTE_SELECTOR_BYTES: usize = 512;
const MAX_ROUTE_VALUE_BYTES: usize = 512;
const MAX_HOME_ROUTE_FILE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidHomeRoutes;

impl std::fmt::Display for InvalidHomeRoutes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the home .ai-memory.toml route map is invalid")
    }
}

impl std::error::Error for InvalidHomeRoutes {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HomeRoute {
    pub(crate) workspace: String,
    pub(crate) project: String,
    pub(crate) identity_style: Option<IdentityStyle>,
    pub(crate) aliases: MarkerAliases,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathSelector {
    root: PathRoot,
    components: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathRoot {
    Posix,
    Drive(String),
    Unc(String, String),
}

impl PathRoot {
    fn same_root(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Posix, Self::Posix) => true,
            (Self::Drive(left), Self::Drive(right)) => left.eq_ignore_ascii_case(right),
            (Self::Unc(left_server, left_share), Self::Unc(right_server, right_share)) => {
                left_server.eq_ignore_ascii_case(right_server)
                    && left_share.eq_ignore_ascii_case(right_share)
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathRoute {
    selector: PathSelector,
    route: HomeRoute,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HomeRoutes {
    identities: Vec<(String, HomeRoute)>,
    paths: Vec<PathRoute>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoutingSource {
    LocalMarker,
    HomeIdentityRoute,
    HomePathRoute,
    HomeRoot,
}

#[derive(Debug, Clone)]
pub(crate) struct RoutingSelection {
    pub(crate) path: PathBuf,
    pub(crate) fields: RoutingFields,
    pub(crate) source: RoutingSource,
    pub(crate) remote_identity: Option<RepositoryIdentity>,
}

pub(crate) type RoutingSelectionResult = Result<Option<RoutingSelection>, InvalidHomeRoutes>;

/// The scope fields a marker declares, plus where it was found.
///
/// `project` is what the marker pins directly; a marker that only sets
/// `project_strategy = "repo-root"` leaves it `None` and lets the caller
/// derive the name (see [`repo_root_project`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MarkerScope {
    /// Absolute path of the marker file the walk settled on.
    pub(crate) path: PathBuf,
    /// `workspace = "…"`.
    pub(crate) workspace: Option<String>,
    /// `project = "…"`.
    pub(crate) project: Option<String>,
    /// `project_strategy = "…"`, or the install-wide env default.
    pub(crate) project_strategy: Option<String>,
    /// Explicit repository identity, which keeps name derivation on the legacy
    /// local path because it is already an operator-chosen coordinate.
    pub(crate) identity: Option<String>,
    /// Explicit remote naming style from marker/home routing. When absent, current
    /// static CLI clients choose and send [`IdentityStyle::Path`]; only omission
    /// at the server/wire boundary defaults to legacy [`IdentityStyle::HostPath`]
    /// for old-client compatibility.
    pub(crate) identity_style: Option<IdentityStyle>,
    /// Normalized remote discovered for the selected checkout, when any.
    pub(crate) remote_identity: Option<RepositoryIdentity>,
}

#[derive(Debug, Clone)]
pub(crate) struct RoutingFields {
    pub(crate) workspace: Option<String>,
    pub(crate) project: Option<String>,
    pub(crate) project_strategy: Option<String>,
    pub(crate) identity: Option<String>,
    pub(crate) identity_style: Option<String>,
    pub(crate) aliases: Result<
        Option<ai_memory_core::repository_identity::MarkerAliases>,
        ai_memory_core::repository_identity::MarkerAliasError,
    >,
}

impl Default for RoutingFields {
    fn default() -> Self {
        Self {
            workspace: None,
            project: None,
            project_strategy: None,
            identity: None,
            identity_style: None,
            aliases: Ok(None),
        }
    }
}

impl RoutingFields {
    fn from_text(text: &str) -> Self {
        Self {
            workspace: parse_key_in(text, "workspace"),
            project: parse_key_in(text, "project"),
            project_strategy: parse_key_in(text, "project_strategy"),
            identity: parse_key_in(text, "identity"),
            identity_style: parse_key_in(text, "identity_style"),
            aliases: parse_aliases_in(text),
        }
    }

    fn apply_route(&mut self, route: &HomeRoute) {
        self.workspace = Some(route.workspace.clone());
        self.project = Some(route.project.clone());
        self.project_strategy = None;
        self.identity = None;
        self.identity_style = route.identity_style.map(|style| style.as_str().to_owned());
        self.aliases = Ok((!route.aliases.is_empty()).then(|| route.aliases.clone()));
    }
}

impl HomeRoutes {
    fn parse(text: &str, home: &Path) -> Result<Self, ()> {
        let saw_route_syntax = text
            .lines()
            .any(|line| recognizable_route_syntax(line.trim()));
        if !saw_route_syntax {
            return Ok(Self::default());
        }
        let mut parsed = Self::default();
        let mut current: Option<(RouteKind, String, RouteFields)> = None;
        let mut raw_selectors = std::collections::HashSet::new();

        for line in text.lines() {
            let trimmed = line.trim();
            if let Some((kind, selector)) = parse_route_header(trimmed) {
                if let Some(route) = current.take() {
                    parsed.push_route(route, home)?;
                }
                if !raw_selectors.insert(selector.clone()) {
                    return Err(());
                }
                current = Some((kind, selector, RouteFields::default()));
                continue;
            }
            if trimmed.starts_with("[routes")
                || trimmed.starts_with("routes.")
                || trimmed
                    .strip_prefix("routes")
                    .is_some_and(|rest| rest.trim_start().starts_with('='))
            {
                return Err(());
            }
            if trimmed.starts_with('[') {
                if let Some(route) = current.take() {
                    parsed.push_route(route, home)?;
                }
                continue;
            }
            if let Some((_, _, fields)) = current.as_mut() {
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                let (key, value) = trimmed.split_once('=').ok_or(())?;
                fields.insert(key.trim(), value.trim())?;
            } else if recognizable_malformed_root_line(trimmed) {
                return Err(());
            }
        }
        if let Some(route) = current {
            parsed.push_route(route, home)?;
        }
        Ok(parsed)
    }

    fn push_route(
        &mut self,
        (kind, raw_selector, fields): (RouteKind, String, RouteFields),
        home: &Path,
    ) -> Result<(), ()> {
        self.check_capacity(&raw_selector)?;
        let route = fields.finish()?;
        match kind {
            RouteKind::Identity => {
                let identity =
                    accept_wire_identity(&raw_selector, IdentitySource::GitRemote.as_str())
                        .ok_or(())?;
                if identity.identity != raw_selector
                    || !valid_identity_selector(&raw_selector)
                    || self
                        .identities
                        .iter()
                        .any(|(existing, _)| existing == &identity.identity)
                {
                    return Err(());
                }
                self.identities.push((identity.identity, route));
            }
            RouteKind::Path => {
                let selector = PathSelector::parse(&raw_selector, home)?;
                if self
                    .paths
                    .iter()
                    .any(|existing| existing.selector.same_selector(&selector))
                {
                    return Err(());
                }
                self.paths.push(PathRoute { selector, route });
            }
        }
        Ok(())
    }

    fn check_capacity(&self, selector: &str) -> Result<(), ()> {
        if self.identities.len() + self.paths.len() >= MAX_HOME_ROUTES
            || selector.is_empty()
            || selector.len() > MAX_ROUTE_SELECTOR_BYTES
            || selector.chars().any(char::is_control)
        {
            return Err(());
        }
        Ok(())
    }

    fn select(
        &self,
        cwd: &str,
        remote: Option<&RepositoryIdentity>,
    ) -> Result<Option<(HomeRoute, RoutingSource)>, ()> {
        if let Some(remote) = remote.filter(|identity| identity.source == IdentitySource::GitRemote)
            && let Some((_, route)) = self
                .identities
                .iter()
                .find(|(selector, _)| selector == &remote.identity)
        {
            return Ok(Some((route.clone(), RoutingSource::HomeIdentityRoute)));
        }
        let cwd = parse_absolute_selector(cwd)?;
        let mut matched: Option<&PathRoute> = None;
        for candidate in self
            .paths
            .iter()
            .filter(|candidate| candidate.selector.contains(&cwd))
        {
            match matched {
                None => matched = Some(candidate),
                Some(current)
                    if candidate.selector.components.len() > current.selector.components.len() =>
                {
                    matched = Some(candidate);
                }
                Some(current)
                    if candidate.selector.components.len() == current.selector.components.len() =>
                {
                    return Err(());
                }
                Some(_) => {}
            }
        }
        Ok(matched.map(|route| (route.route.clone(), RoutingSource::HomePathRoute)))
    }
}

impl PathSelector {
    /// A drive-letter or UNC root names a filesystem that compares names
    /// case-insensitively, so its components must fold ASCII case.
    fn is_windows(&self) -> bool {
        matches!(self.root, PathRoot::Drive(_) | PathRoot::Unc(_, _))
    }

    fn same_selector(&self, other: &Self) -> bool {
        if !self.root.same_root(&other.root) || self.components.len() != other.components.len() {
            return false;
        }
        let windows = self.is_windows();
        self.components
            .iter()
            .zip(&other.components)
            .all(|(left, right)| {
                if windows {
                    left.eq_ignore_ascii_case(right)
                } else {
                    left == right
                }
            })
    }

    fn parse(raw: &str, home: &Path) -> Result<Self, ()> {
        if raw.is_empty()
            || raw.len() > MAX_ROUTE_SELECTOR_BYTES
            || raw.chars().any(char::is_control)
        {
            return Err(());
        }
        if let Some(rest) = raw.strip_prefix("~/") {
            let mut depth = 0usize;
            for component in rest.replace('\\', "/").split('/') {
                match component {
                    "" | "." => {}
                    ".." if depth == 0 => return Err(()),
                    ".." => depth -= 1,
                    _ => depth += 1,
                }
            }
            return parse_absolute_selector(&format!(
                "{}/{}",
                home.to_string_lossy().trim_end_matches(['/', '\\']),
                rest
            ));
        }
        parse_absolute_selector(raw)
    }

    fn contains(&self, cwd: &Self) -> bool {
        if !self.root.same_root(&cwd.root) || self.components.len() > cwd.components.len() {
            return false;
        }
        let windows = self.is_windows();
        self.components
            .iter()
            .zip(&cwd.components)
            .all(|(left, right)| {
                if windows {
                    left.eq_ignore_ascii_case(right)
                } else {
                    left == right
                }
            })
    }
}

/// Whether `root` contains `candidate`, comparing whole components and
/// folding ASCII case under a Windows root — the rule [`PathSelector`]
/// already applies to the home marker's path routes.
///
/// `pub(crate)`: `server_profiles::resolve` admits a marker against a
/// profile's `roots` with the same rule, so one checkout cannot be routed by
/// a path route yet refused by a root that covers it. `Path::starts_with`
/// alone compares components byte-exactly, which on a case-insensitive
/// filesystem rejects `C:\work\team-b` against a root spelled
/// `C:\Work\team-b`. A path that is not a Windows selector keeps the
/// byte-exact comparison rather than being folded.
pub(crate) fn path_contains(root: &Path, candidate: &Path) -> bool {
    if let Ok(root_selector) = parse_absolute_selector(&root.to_string_lossy())
        && root_selector.is_windows()
        && let Ok(candidate_selector) = parse_absolute_selector(&candidate.to_string_lossy())
    {
        return root_selector.contains(&candidate_selector);
    }
    candidate.starts_with(root)
}

fn parse_absolute_selector(raw: &str) -> Result<PathSelector, ()> {
    let replaced = raw.replace('\\', "/");
    let (root, rest) = if let Some(rest) = replaced.strip_prefix("//") {
        let mut parts = rest.split('/').filter(|part| !part.is_empty());
        let server = parts.next().ok_or(())?;
        let share = parts.next().ok_or(())?;
        (
            PathRoot::Unc(server.to_ascii_lowercase(), share.to_ascii_lowercase()),
            parts.collect::<Vec<_>>(),
        )
    } else if replaced.as_bytes().get(1) == Some(&b':')
        && replaced.as_bytes().get(2) == Some(&b'/')
        && replaced.as_bytes()[0].is_ascii_alphabetic()
    {
        (
            PathRoot::Drive(replaced[..1].to_ascii_uppercase()),
            replaced[3..]
                .split('/')
                .filter(|part| !part.is_empty())
                .collect(),
        )
    } else if let Some(rest) = replaced.strip_prefix('/') {
        (
            PathRoot::Posix,
            rest.split('/').filter(|part| !part.is_empty()).collect(),
        )
    } else {
        return Err(());
    };
    let mut components: Vec<String> = Vec::new();
    for component in rest {
        match component {
            "." => {}
            ".." => {
                components.pop();
            }
            value => components.push(value.to_owned()),
        }
    }
    if components.is_empty() {
        return Err(());
    }
    Ok(PathSelector { root, components })
}

fn valid_identity_selector(value: &str) -> bool {
    let mut segments = value.split('/');
    let Some(host) = segments.next() else {
        return false;
    };
    !host.starts_with('.')
        && !host.ends_with('.')
        && host.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        && segments.clone().count() >= 1
        && segments.all(|segment| {
            !segment.is_empty()
                && segment.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'_' | b'-')
                })
        })
}

fn valid_route_name(value: &str) -> bool {
    value.len() <= MAX_ROUTE_VALUE_BYTES
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
        })
}

#[derive(Debug, Clone, Copy)]
enum RouteKind {
    Identity,
    Path,
}

#[derive(Debug, Default)]
struct RouteFields {
    workspace: Option<String>,
    project: Option<String>,
    identity_style: Option<String>,
    aliases: Option<String>,
}

fn recognizable_route_syntax(line: &str) -> bool {
    line.starts_with("[routes")
        || line.starts_with("routes.")
        || line
            .strip_prefix("routes")
            .is_some_and(|rest| rest.trim_start().starts_with('='))
        || line.starts_with("route_")
}

fn recognizable_malformed_root_line(line: &str) -> bool {
    if line.is_empty() || line.starts_with('#') {
        return false;
    }
    if line.starts_with("route_") {
        return true;
    }
    let Some((key, value)) = line.split_once('=') else {
        return false;
    };
    let key = key.trim();
    let value = value.trim();
    matches!(
        key,
        "workspace"
            | "project"
            | "project_strategy"
            | "drop_subagent_captures"
            | "identity"
            | "identity_style"
            | "server"
    ) && value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .is_none_or(|inner| inner.contains('"'))
}

fn parse_route_header(line: &str) -> Option<(RouteKind, String)> {
    let body = line.strip_prefix("[routes.")?.strip_suffix(']')?;
    let (kind, quoted) = body.split_once('.')?;
    let selector = quoted.strip_prefix('"')?.strip_suffix('"')?;
    if selector.contains(['"', '\\']) {
        return None;
    }
    let kind = match kind {
        "identity" => RouteKind::Identity,
        "path" => RouteKind::Path,
        _ => return None,
    };
    Some((kind, selector.to_owned()))
}

impl RouteFields {
    fn insert(&mut self, key: &str, value: &str) -> Result<(), ()> {
        let slot = match key {
            "route_workspace" => &mut self.workspace,
            "route_project" => &mut self.project,
            "route_identity_style" => &mut self.identity_style,
            "route_aliases" => &mut self.aliases,
            _ => return Err(()),
        };
        if slot.replace(value.to_owned()).is_some() {
            return Err(());
        }
        Ok(())
    }

    fn finish(self) -> Result<HomeRoute, ()> {
        let string_value = |raw: Option<String>| {
            let raw = raw.ok_or(())?;
            if raw.len() > MAX_ROUTE_VALUE_BYTES + 2 || raw.contains('\\') {
                return Err(());
            }
            let value = raw
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .ok_or(())?;
            if value.is_empty() || value.len() > MAX_ROUTE_VALUE_BYTES || value.contains('"') {
                return Err(());
            }
            Ok(value.to_owned())
        };
        let workspace = string_value(self.workspace)?;
        let project = string_value(self.project)?;
        if !valid_route_name(&workspace) || !valid_route_name(&project) {
            return Err(());
        }
        let identity_style = self
            .identity_style
            .map(|raw| string_value(Some(raw)))
            .transpose()?
            .map(|style| IdentityStyle::from_str_opt(&style).ok_or(()))
            .transpose()?;
        let aliases = match self.aliases {
            None => MarkerAliases::default(),
            Some(raw) => {
                if raw.len() > MAX_ROUTE_VALUE_BYTES || raw.contains('\\') {
                    return Err(());
                }
                let marker = format!("aliases = {raw}");
                ai_memory_core::repository_identity::parse_marker_aliases(&marker)
                    .map_err(|_| ())?
                    .ok_or(())?
            }
        };
        Ok(HomeRoute {
            workspace,
            project,
            identity_style,
            aliases,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MarkerInspection {
    pub(crate) status: &'static str,
    pub(crate) fields: RoutingFields,
    pub(crate) scope: Option<MarkerScope>,
    pub(crate) route_identity: Option<RepositoryIdentity>,
}

#[cfg(test)]
pub(crate) fn inspect_scope(cwd: &str, env: &RuntimeEnv) -> MarkerInspection {
    inspect_scope_for(cwd, cwd, env)
}

pub(crate) fn inspect_scope_for(
    lookup_cwd: &str,
    identity_cwd: &str,
    env: &RuntimeEnv,
) -> MarkerInspection {
    use std::io::Read as _;
    let empty = |status| MarkerInspection {
        status,
        fields: RoutingFields::default(),
        scope: None,
        route_identity: None,
    };
    if env.ignore_marker() {
        return empty("ignored");
    }
    let selection = match routing_selection_with_home(
        lookup_cwd,
        identity_cwd,
        env.home_dir().map(Path::new),
        false,
    ) {
        Ok(Some(selection)) => selection,
        Err(_) => return empty("invalid_home_routes"),
        Ok(None) => {
            if let Some(path) = find_marker_with_home(lookup_cwd, env.home_dir().map(Path::new))
                && std::fs::read_to_string(path)
                    .is_ok_and(|text| text.parse::<toml_edit::DocumentMut>().is_err())
            {
                return empty("invalid");
            }
            return empty("absent");
        }
    };
    let limit = ai_memory_hooks::capture_policy::MAX_MARKER_BYTES;
    let text = (|| {
        let mut bytes = Vec::new();
        std::fs::File::open(&selection.path)
            .ok()?
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        (bytes.len() <= limit).then_some(())?;
        String::from_utf8(bytes).ok()
    })();
    let Some(text) = text else {
        return empty("unreadable");
    };
    let Ok(document) = text.parse::<toml_edit::DocumentMut>() else {
        return empty("invalid");
    };
    let route_identity = selection.remote_identity;
    let fields = selection.fields;
    if matches!(
        selection.source,
        RoutingSource::LocalMarker | RoutingSource::HomeRoot
    ) {
        for (key, parsed) in [
            ("workspace", &fields.workspace),
            ("project", &fields.project),
            ("project_strategy", &fields.project_strategy),
            ("identity", &fields.identity),
            ("identity_style", &fields.identity_style),
        ] {
            let strict = document.get(key).and_then(toml_edit::Item::as_str);
            if strict != parsed.as_deref()
                || document
                    .get(key)
                    .is_some_and(|item| item.as_str().is_none())
            {
                return empty("conflicting");
            }
        }
    }
    if fields.aliases.is_err()
        || fields
            .identity_style
            .as_deref()
            .is_some_and(|style| IdentityStyle::from_str_opt(style).is_none())
        || fields
            .project_strategy
            .as_deref()
            .is_some_and(|strategy| !matches!(strategy, "repo-root" | "repo_root" | "basename"))
    {
        return empty("conflicting");
    }
    let scope = MarkerScope {
        path: selection.path,
        workspace: fields.workspace.clone(),
        project: fields.project.clone(),
        project_strategy: fields.project_strategy.clone().or_else(|| {
            env.project_strategy()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
        }),
        identity: fields.identity.clone(),
        identity_style: fields
            .identity_style
            .as_deref()
            .and_then(IdentityStyle::from_str_opt),
        remote_identity: route_identity.clone(),
    };
    MarkerInspection {
        status: match selection.source {
            RoutingSource::HomeIdentityRoute => "home_identity_route",
            RoutingSource::HomePathRoute => "home_path_route",
            _ => "valid",
        },
        fields,
        scope: scope.declares_scope().then_some(scope),
        route_identity,
    }
}

impl MarkerScope {
    /// Whether this marker declares anything that changes scope resolution.
    /// A marker that only carries `[capture]` rules does not.
    pub(crate) fn declares_scope(&self) -> bool {
        self.workspace.is_some()
            || self.project.is_some()
            || self.identity.is_some()
            || self.identity_style.is_some()
            || self.remote_identity.is_some()
            || self.is_repo_root()
    }

    pub(crate) fn canonical_remote_project(&self) -> Option<String> {
        if self.project.is_some() || self.identity.is_some() {
            return None;
        }
        let repository = self.remote_identity.as_ref()?;
        (self.identity_style.unwrap_or(IdentityStyle::Path) == IdentityStyle::Path)
            .then(|| ai_memory_core::repository_identity::path_style_name(repository))
            .flatten()
    }

    /// Whether the effective strategy asks for repo-root project naming.
    /// Accepts both spellings, matching the shell hooks.
    pub(crate) fn is_repo_root(&self) -> bool {
        matches!(
            self.project_strategy.as_deref(),
            Some("repo-root" | "repo_root")
        )
    }
}

/// Read the nearest marker's scope declaration, honouring
/// `AI_MEMORY_IGNORE_MARKER`.
///
/// Both env knobs arrive through [`RuntimeEnv`] rather than being read here:
/// `Config::load()` is the one config-read path, and routing them through it
/// is also what makes the caller's unit tests hermetic against a developer's
/// exported `AI_MEMORY_IGNORE_MARKER`.
///
/// Returns `None` when no marker is found, when the operator disabled marker
/// resolution, or when the marker declares nothing scope-related — callers
/// then keep their existing fallbacks untouched.
pub(crate) fn read_scope(
    cwd: &str,
    env: &RuntimeEnv,
) -> Result<Option<MarkerScope>, InvalidHomeRoutes> {
    read_scope_for(cwd, cwd, env)
}

pub(crate) fn read_scope_for(
    lookup_cwd: &str,
    identity_cwd: &str,
    env: &RuntimeEnv,
) -> Result<Option<MarkerScope>, InvalidHomeRoutes> {
    let Some(selection) = routing_selection_with_home(
        lookup_cwd,
        identity_cwd,
        env.home_dir().map(Path::new),
        env.ignore_marker(),
    )?
    else {
        return Ok(None);
    };
    let mut scope = MarkerScope {
        workspace: selection.fields.workspace,
        project: selection.fields.project,
        project_strategy: selection.fields.project_strategy,
        identity: selection.fields.identity,
        identity_style: selection
            .fields
            .identity_style
            .as_deref()
            .and_then(IdentityStyle::from_str_opt),
        remote_identity: selection
            .remote_identity
            .or_else(|| discover_remote_identity(identity_cwd))
            .or_else(|| {
                (lookup_cwd != identity_cwd)
                    .then(|| discover_remote_identity(lookup_cwd))
                    .flatten()
            }),
        path: selection.path,
    };
    if scope.project_strategy.is_none() {
        scope.project_strategy = env
            .project_strategy()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned);
    }
    Ok(scope.declares_scope().then_some(scope))
}

pub(crate) fn routing_selection(cwd: &str) -> RoutingSelectionResult {
    routing_selection_for(cwd, cwd)
}

pub(crate) fn routing_selection_for(
    lookup_cwd: &str,
    identity_cwd: &str,
) -> RoutingSelectionResult {
    routing_selection_with_home(lookup_cwd, identity_cwd, home_dir().as_deref(), false)
}

pub(crate) fn routing_selection_with_home(
    lookup_cwd: &str,
    identity_cwd: &str,
    home: Option<&Path>,
    ignore_marker: bool,
) -> RoutingSelectionResult {
    if ignore_marker {
        return Ok(None);
    }
    let local = find_settings_marker_with_home(lookup_cwd, home).filter(|path| {
        home.is_none_or(|home| {
            absolute_normalized(path) != absolute_normalized(&home.join(MARKER_FILENAME))
        })
    });
    if let Some(path) = local {
        let text = std::fs::read_to_string(&path).ok();
        let Some(text) = text else {
            return Ok(None);
        };
        let fields = RoutingFields::from_text(&text);
        let remote_identity = if fields
            .aliases
            .as_ref()
            .is_ok_and(|aliases| aliases.is_some())
        {
            discover_remote_identity(identity_cwd).or_else(|| {
                (lookup_cwd != identity_cwd)
                    .then(|| discover_remote_identity(lookup_cwd))
                    .flatten()
            })
        } else {
            None
        };
        return Ok(Some(RoutingSelection {
            path,
            fields,
            source: RoutingSource::LocalMarker,
            remote_identity,
        }));
    }
    let Some(home) = home else {
        return Ok(None);
    };
    let path = home.join(MARKER_FILENAME);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(InvalidHomeRoutes),
    };
    let expected = file.metadata().map_err(|_| InvalidHomeRoutes)?.len();
    if expected > MAX_HOME_ROUTE_FILE_BYTES as u64 {
        return Err(InvalidHomeRoutes);
    }
    let mut bytes = Vec::with_capacity(MAX_HOME_ROUTE_FILE_BYTES + 1);
    use std::io::Read as _;
    (&file)
        .take((MAX_HOME_ROUTE_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| InvalidHomeRoutes)?;
    if bytes.len() > MAX_HOME_ROUTE_FILE_BYTES
        || bytes.len() as u64 != expected
        || file.metadata().map_err(|_| InvalidHomeRoutes)?.len() != expected
    {
        return Err(InvalidHomeRoutes);
    }
    let text = String::from_utf8(bytes).map_err(|_| InvalidHomeRoutes)?;
    let mut fields = RoutingFields::from_text(&text);
    let remote_identity = discover_remote_identity(identity_cwd).or_else(|| {
        (lookup_cwd != identity_cwd)
            .then(|| discover_remote_identity(lookup_cwd))
            .flatten()
    });
    let routes = HomeRoutes::parse(&text, home).map_err(|_| InvalidHomeRoutes)?;
    match routes.select(identity_cwd, remote_identity.as_ref()) {
        Ok(Some((route, source))) => {
            fields.apply_route(&route);
            return Ok(Some(RoutingSelection {
                path,
                fields,
                source,
                remote_identity,
            }));
        }
        Err(()) => return Err(InvalidHomeRoutes),
        Ok(None) => {}
    }
    Ok(Some(RoutingSelection {
        path,
        fields,
        source: RoutingSource::HomeRoot,
        remote_identity,
    }))
}

pub(crate) fn discover_remote_identity(cwd: &str) -> Option<RepositoryIdentity> {
    let (upstream, origin) = ai_memory_consolidate::read_identity_remotes(Path::new(cwd));
    [upstream.as_deref(), origin.as_deref()]
        .into_iter()
        .flatten()
        .find_map(ai_memory_core::repository_identity::normalize_remote_url)
        .map(|identity| RepositoryIdentity {
            identity,
            source: IdentitySource::GitRemote,
        })
}

/// Derive a project name from the **main** repository root, so linked
/// worktrees and subdirectories collapse onto one project.
pub(crate) fn repo_root_project(cwd: &str) -> Option<String> {
    let root = ai_memory_consolidate::discover_main_repo_root(Path::new(cwd)).ok()?;
    root.file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Walk up from `cwd` toward `$HOME` looking for `.ai-memory.toml`.
/// Checkouts outside `$HOME` stop at their nearest `.git` root; a non-git
/// directory outside `$HOME` checks only `cwd`. When home is unavailable the
/// historical filesystem-root walk remains the fallback.
pub(crate) fn find_marker(cwd: &str) -> Option<PathBuf> {
    let home = home_dir();
    find_marker_with_home(cwd, home.as_deref())
}

fn find_marker_with_home(cwd: &str, home: Option<&Path>) -> Option<PathBuf> {
    find_marker_matching(cwd, home, OutsideHome::StopAtCheckoutRoot, |path| {
        Some(path.to_path_buf())
    })
}

/// Like [`find_marker`], but skips a marker that declares nothing beyond a
/// `[capture]` section (scope/settings-*transparent*) and continues the walk
/// to the next ancestor. Resolves `workspace`/`project`/`project_strategy`
/// and the other root-level settings `hook_capture` forwards, so a nested
/// capture-only marker no longer resets them to their fallback (#668).
/// `[capture]`/`ignore_paths` itself keeps using [`find_marker`] — the
/// nearest marker, unchanged.
pub(crate) fn find_settings_marker(cwd: &str) -> Option<PathBuf> {
    let home = home_dir();
    find_settings_marker_with_home(cwd, home.as_deref())
}

fn find_settings_marker_with_home(cwd: &str, home: Option<&Path>) -> Option<PathBuf> {
    let home_marker = home.map(|home| absolute_normalized(&home.join(MARKER_FILENAME)));
    find_marker_matching(cwd, home, OutsideHome::StopAtCheckoutRoot, |path| {
        if home_marker
            .as_ref()
            .is_some_and(|marker| absolute_normalized(path) == *marker)
        {
            return Some(path.to_path_buf());
        }
        std::fs::read_to_string(path)
            .is_ok_and(|text| declares_more_than_capture(&text))
            .then(|| path.to_path_buf())
    })
}

/// Where a walk that starts outside `$HOME` stops.
#[derive(Clone, Copy)]
enum OutsideHome {
    /// At the nearest `.git` root, or `cwd` itself outside any checkout.
    StopAtCheckoutRoot,
    /// At the filesystem root.
    WalkToRoot,
}

/// Shared walk-up-from-`cwd`-toward-`$HOME` used by every marker lookup;
/// `matches` maps a marker file found along the way to a result that stops
/// the walk, or `None` to continue to the next ancestor. Inside `$HOME` the
/// walk stops at `$HOME`; `outside_home` picks the stop for a start outside it.
fn find_marker_matching<T>(
    cwd: &str,
    home: Option<&Path>,
    outside_home: OutsideHome,
    mut matches: impl FnMut(&Path) -> Option<T>,
) -> Option<T> {
    let start = absolute_normalized(Path::new(cwd));
    let home = home.map(absolute_normalized);
    let boundary = match (home.as_deref(), outside_home) {
        (Some(home), _) if start.starts_with(home) => Some(home.to_path_buf()),
        (Some(_), OutsideHome::StopAtCheckoutRoot) => {
            Some(checkout_root(&start).unwrap_or_else(|| start.clone()))
        }
        (Some(_), OutsideHome::WalkToRoot) | (None, _) => None,
    };

    let mut dir = start.as_path();
    loop {
        let candidate = dir.join(".ai-memory.toml");
        if candidate.is_file()
            && let Some(found) = matches(&candidate)
        {
            return Some(found);
        }
        if boundary.as_deref() == Some(dir) {
            return None;
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent,
            _ => return None,
        }
    }
}

/// [`find_marker`] for the many directories one event names: every directory
/// a lookup passes through keeps its answer (and its [`checkout_root`]), so a
/// deep tree costs one probe per directory rather than one walk per lookup.
/// Each filesystem probe first calls the caller's `spend`; when that refuses,
/// the lookup stops with `None`.
pub(crate) struct MarkerMemo {
    home: Option<PathBuf>,
    governing: std::collections::HashMap<PathBuf, Option<PathBuf>>,
    checkouts: std::collections::HashMap<PathBuf, Option<PathBuf>>,
    /// Directories known to exist: one a probe found, and every ancestor of
    /// it.
    directories: std::collections::HashSet<PathBuf>,
}

impl MarkerMemo {
    pub(crate) fn with_home(home: Option<&Path>) -> Self {
        Self {
            home: home.map(absolute_normalized),
            governing: std::collections::HashMap::new(),
            checkouts: std::collections::HashMap::new(),
            directories: std::collections::HashSet::new(),
        }
    }

    /// Records that `dir` exists, and with it every ancestor.
    pub(crate) fn note_directory(&mut self, dir: &Path) {
        for ancestor in absolute_normalized(dir).ancestors() {
            if !self.directories.insert(ancestor.to_path_buf()) {
                break;
            }
        }
    }

    /// The nearest existing directory at or above `path` (an existing path's
    /// parent when it is not a directory), probing only what is not known
    /// already; `path` is taken as given, never joined to the process's own
    /// directory; `Some(None)` when there is none, `None` when `spend` refuses
    /// a probe first.
    pub(crate) fn existing_dir(
        &mut self,
        path: &Path,
        spend: &mut dyn FnMut() -> Option<()>,
    ) -> Option<Option<PathBuf>> {
        let mut candidate = path;
        loop {
            if self.directories.contains(candidate) {
                return Some(Some(candidate.to_path_buf()));
            }
            spend()?;
            if candidate.is_dir() {
                self.note_directory(candidate);
                return Some(Some(candidate.to_path_buf()));
            }
            match candidate.parent() {
                Some(parent) => candidate = parent,
                None => return Some(None),
            }
        }
    }

    /// The marker [`find_marker`] finds from `dir`; `None` when `spend`
    /// refuses a probe first.
    pub(crate) fn find_marker(
        &mut self,
        dir: &Path,
        spend: &mut dyn FnMut() -> Option<()>,
    ) -> Option<Option<PathBuf>> {
        let mut current = absolute_normalized(dir);
        let mut chain = Vec::new();
        let found = loop {
            if let Some(known) = self.governing.get(&current) {
                break known.clone();
            }
            spend()?;
            let candidate = current.join(MARKER_FILENAME);
            if candidate.is_file() {
                chain.push(current);
                break Some(candidate);
            }
            // The same boundary as `find_marker_matching`: `$HOME` for a
            // directory inside it, else the nearest `.git` root (the
            // directory itself outside any checkout), else the filesystem
            // root when home is unknown.
            let boundary = match self.home.as_deref() {
                Some(home) if current.starts_with(home) => current == home,
                // Below a directory known to lie in no checkout, this one is
                // a checkout root or in none: a boundary either way.
                Some(_) => {
                    current
                        .parent()
                        .is_some_and(|parent| matches!(self.checkouts.get(parent), Some(None)))
                        || self
                            .checkout_root(&current, spend)?
                            .is_none_or(|root| root == current)
                }
                None => false,
            };
            let parent = current.parent().map(Path::to_path_buf);
            chain.push(current);
            match parent {
                Some(parent)
                    if !boundary
                        && Some(parent.as_path()) != chain.last().map(PathBuf::as_path) =>
                {
                    current = parent;
                }
                _ => break None,
            }
        };
        for dir in chain {
            self.governing.insert(dir, found.clone());
        }
        Some(found)
    }

    /// Records the marker an earlier [`find_marker`] found from `dir`, for
    /// `dir` and every directory that walk passed: up to the marker's own
    /// directory, or to the walk's boundary when it found none. Call it after
    /// [`Self::checkout_root`] of `dir`, which that boundary may need.
    pub(crate) fn remember(&mut self, dir: &Path, marker: Option<&Path>) {
        let start = absolute_normalized(dir);
        let stop = match marker {
            Some(marker) => marker.parent().map(absolute_normalized),
            None => match self.home.as_deref() {
                Some(home) if start.starts_with(home) => Some(home.to_path_buf()),
                Some(_) => self.checkouts.get(&start).cloned().flatten(),
                None => None,
            },
        };
        // Outside a checkout (or for a marker not above `dir`) the walk
        // checked `dir` alone.
        let stop = match (marker, &stop) {
            (_, Some(stop)) if start.starts_with(stop) => Some(stop.clone()),
            (None, None) if self.home.is_none() => None,
            _ => Some(start.clone()),
        };
        let mut current = Some(start.as_path());
        while let Some(dir) = current {
            self.governing
                .insert(dir.to_path_buf(), marker.map(Path::to_path_buf));
            if Some(dir) == stop.as_deref() {
                break;
            }
            current = dir.parent();
        }
    }

    /// [`checkout_root`] of `dir`, remembered for every directory passed.
    pub(crate) fn checkout_root(
        &mut self,
        dir: &Path,
        spend: &mut dyn FnMut() -> Option<()>,
    ) -> Option<Option<PathBuf>> {
        let mut current = absolute_normalized(dir);
        let mut chain = Vec::new();
        let found = loop {
            if let Some(known) = self.checkouts.get(&current) {
                break known.clone();
            }
            spend()?;
            if current.join(".git").exists() {
                let root = current.clone();
                chain.push(current);
                break Some(root);
            }
            let parent = current.parent().map(Path::to_path_buf);
            chain.push(current);
            match parent {
                Some(parent) if Some(parent.as_path()) != chain.last().map(PathBuf::as_path) => {
                    current = parent;
                }
                _ => break None,
            }
        };
        for dir in chain {
            self.checkouts.insert(dir, found.clone());
        }
        Some(found)
    }
}

/// Whether a marker's raw text declares anything beyond a `[capture]`
/// section: any root-level scope key (`workspace`/`project`/
/// `project_strategy`), or any of the other settings
/// `hook_capture::marker_query_suffix_impl` forwards (`[recall]
/// default_global`, `[briefing]` and `[profile]` keys, top-level
/// `drop_subagent_captures`).
/// A marker with any of these is a resolution boundary; only a marker whose
/// only content is `[capture]` (e.g. `ignore_paths`) is transparent (#668).
///
/// Line-based like [`parse_key_in`] / [`parse_toml_flag`] — section headers
/// are not tracked, so a stray key is still detected wherever it appears in
/// the file. That is conservative on purpose: it can only turn a marker INTO
/// a boundary, never wrongly make one transparent.
fn declares_more_than_capture(text: &str) -> bool {
    const QUOTED_KEYS: [&str; 6] = [
        "workspace",
        "project",
        "project_strategy",
        "drop_subagent_captures",
        "identity",
        "identity_style",
    ];
    const FLAG_KEYS: [&str; 7] = [
        "default_global",
        "inject_on_session_start",
        "inject_on_subagent_start",
        "max_chars",
        "contribute",
        "consume",
        "deliver_on_session_start",
    ];
    QUOTED_KEYS
        .iter()
        .any(|key| parse_key_in(text, key).is_some())
        || FLAG_KEYS
            .iter()
            .any(|key| parse_flag_in(text, key).is_some())
        || server_selection_in(text).is_some()
        || text.lines().any(|line| {
            line.trim_start_matches('\u{feff}')
                .trim_start()
                .strip_prefix("aliases")
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        })
}

/// A marker's `server = "<profile>"` selection (#992), and the directory of
/// the marker that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerSelection {
    /// The raw value, validated later by `server_profiles::ProfileName`;
    /// `None` when a marker on the walk exists but could not be read, which
    /// the caller must treat as a refused selection, never as "no selection".
    pub(crate) name: Option<String>,
    /// Directory holding the declaring marker, lexically normalized.
    pub(crate) marker_dir: PathBuf,
}

/// The nearest marker on the walk from `cwd` that declares `server`.
///
/// Deliberately *not* [`find_settings_marker`]'s nearest-marker rule, in two
/// ways, both of which would otherwise route a profile's capture to the
/// install-default server:
///
/// - Routing is inherited down the tree: a nested marker that sets only
///   `workspace` must not reset a subdirectory of a profile-routed tree. A
///   nested marker can only select a different profile, which that
///   profile's `roots` then have to admit.
/// - Outside `$HOME` the walk does not stop at the checkout root, so an
///   organisation-level marker above a repository (`/srv/work/team-b/`,
///   `/Volumes/…`) still routes it. A marker planted higher up can only name
///   a profile this operator registered, which `roots` gate once there are
///   several.
///
/// It also fails closed on content: a marker it cannot read is a refused
/// selection, and a UTF-8 BOM or stray non-UTF-8 byte cannot hide the key.
///
/// `home` is the walk boundary inside `$HOME`; the caller passes the one it
/// also expands `~/` roots against.
pub(crate) fn find_server_selection(cwd: &str, home: Option<&Path>) -> Option<ServerSelection> {
    find_marker_matching(cwd, home, OutsideHome::WalkToRoot, |path| {
        let name = match std::fs::read(path) {
            Ok(bytes) => Some(server_selection_in(&String::from_utf8_lossy(&bytes))?),
            Err(_) => None,
        };
        Some(ServerSelection {
            name,
            marker_dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
        })
    })
}

/// Line-based like [`parse_key_in`], but it fails closed on shape: a
/// `server = team-b` without quotes, or an empty `server = ""`, still counts
/// as a selection (and is then rejected by name validation) instead of being
/// ignored and silently delivered to the install default. Section headers are
/// not tracked, so a `server` key under any table is treated the same way.
fn server_selection_in(text: &str) -> Option<String> {
    for line in text.lines() {
        // A BOM is not whitespace to `trim_start`, and would hide a first-line key.
        let line = line.trim_start_matches('\u{feff}').trim_start();
        let Some(rest) = line.strip_prefix("server") else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim();
        let value = match value.strip_prefix('"') {
            Some(quoted) => quoted.split_once('"').map_or(quoted, |(inner, _)| inner),
            None => value.split('#').next().unwrap_or("").trim(),
        };
        return Some(value.to_owned());
    }
    None
}

/// Make `path` absolute and resolve its `.`/`..` components, WITHOUT
/// touching the filesystem: no symlink resolution, no `\\?\` verbatim
/// prefix. `find_marker`'s callers (`hook_capture::capture_policy` ->
/// `CapturePolicy::compile`) compare the returned marker directory, as a
/// plain string prefix, against runtime candidate paths straight from the
/// hook payload (`cwd`/`tool_input.file_path`) — paths the hook host never
/// canonicalizes. A `fs::canonicalize`-based normalization here used to
/// resolve macOS's `/var` -> `/private/var` symlink and prepend Windows'
/// `\\?\` prefix, moving the marker path into a different namespace than
/// the candidate and making `[capture] ignore_paths` glob matching
/// silently miss on both platforms (#671). Lexical normalization keeps
/// `start`, `home`, the marker path and `checkout_root` in the caller's own
/// namespace on every platform, and still resolves `..` traversal purely
/// syntactically, preserving the boundary hardening from f69e896e.
///
/// `pub(crate)`: `commands::hook` normalizes the same raw hook `cwd` with
/// this exact function (see `commands::hook::lexical_capture_cwd`) before
/// joining a tool event's relative candidate path onto it, so the join lands
/// in the identical namespace as the marker directory found here — a
/// symlinked cwd (e.g. #671's `capture_drop_handles_symlinked_cwd`) then
/// matches `ignore_paths` without either side resolving the symlink.
pub(crate) fn absolute_normalized(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    lexically_normalize(&absolute)
}

/// Resolve `.`/`..` path components purely syntactically (no filesystem
/// access) — the well-known `path-clean` algorithm. A leading `..` that has
/// nothing left to pop (already at a root, or a still-relative path with no
/// preceding `Normal` component) is kept rather than dropped or erroring,
/// matching `canonicalize`'s inability to go above `/`.
fn lexically_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut stack: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match stack.last() {
                Some(Component::Normal(_)) => {
                    stack.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => stack.push(component),
            },
            other => stack.push(other),
        }
    }
    stack.into_iter().collect()
}

pub(crate) fn checkout_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent,
            _ => return None,
        }
    }
}

/// Parse a root-level `key = "value"` line (no nesting, arrays, or
/// tables), mirroring `ai_memory_parse_toml_key`. Returns the first
/// match. Avoids pulling in a TOML parser dependency.
pub(crate) fn parse_toml_key(file: &Path, key: &str) -> Option<String> {
    parse_key_in(&std::fs::read_to_string(file).ok()?, key)
}

/// [`parse_toml_key`] over already-read marker text, so a caller that needs
/// several keys pays for one read.
fn parse_key_in(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        // A BOM is not whitespace to `trim_start`, and would hide a first-line key.
        let trimmed = line.trim_start_matches('\u{feff}').trim_start();
        let Some(after_key) = trimmed.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = after_key.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        if let Some(end) = rest.find('"') {
            return Some(rest[..end].to_string());
        }
    }
    None
}

fn parse_aliases_in(
    text: &str,
) -> Result<
    Option<ai_memory_core::repository_identity::MarkerAliases>,
    ai_memory_core::repository_identity::MarkerAliasError,
> {
    ai_memory_core::repository_identity::parse_marker_aliases(text)
}

/// Parse a root-level `key = <value>` line, accepting a quoted string
/// (`key = "true"`) OR a bare token (`key = true` / `key = 1`), so a
/// `[recall] default_global = true` marker works whether or not the operator
/// quotes the value. Line-based like [`parse_toml_key`], so section headers
/// are ignored; strips an optional trailing `# comment`.
pub(crate) fn parse_toml_flag(file: &Path, key: &str) -> Option<String> {
    parse_flag_in(&std::fs::read_to_string(file).ok()?, key)
}

/// [`parse_toml_flag`] over already-read marker text — the counterpart to
/// [`parse_key_in`], shared with [`declares_more_than_capture`] so it pays
/// for one read per key instead of re-opening the file.
fn parse_flag_in(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| parse_flag_line(line, key))
}

fn parse_flag_line(line: &str, key: &str) -> Option<String> {
    // A BOM is not whitespace to `trim_start`, and would hide a first-line key.
    let trimmed = line.trim_start_matches('\u{feff}').trim_start();
    let after_key = trimmed.strip_prefix(key)?;
    let rest = after_key.trim_start().strip_prefix('=')?;
    let val = rest
        .split('#')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('"');
    (!val.is_empty()).then(|| val.to_string())
}

/// Parse a flag only inside one TOML table. Used for `[recovery]`, whose key
/// deliberately matches the independent `[handoff]` delivery setting.
pub(crate) fn parse_toml_section_flag(file: &Path, section: &str, key: &str) -> Option<String> {
    parse_scoped_flag_in(&std::fs::read_to_string(file).ok()?, section, key, false)
}

/// Preserve the section-blind handoff parser everywhere except the new
/// `[recovery]` table, whose same-named key must not disable handoffs.
pub(crate) fn parse_toml_flag_outside_section(
    file: &Path,
    section: &str,
    key: &str,
) -> Option<String> {
    parse_scoped_flag_in(&std::fs::read_to_string(file).ok()?, section, key, true)
}

fn parse_scoped_flag_in(text: &str, section: &str, key: &str, exclude: bool) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let trimmed = line.trim_start_matches('\u{feff}').trim();
        let header = trimmed.split('#').next().unwrap_or("").trim();
        if let Some(name) = header
            .strip_prefix('[')
            .and_then(|header| header.strip_suffix(']'))
        {
            in_section = name.trim() == section;
            continue;
        }
        if in_section == exclude {
            continue;
        }
        if let Some(value) = parse_flag_line(line, key) {
            return Some(value);
        }
    }
    None
}

/// Shell-parity truthiness for marker flags and the ignore switch.
pub(crate) fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// An explicitly falsy marker flag, for settings that stay on unless turned
/// off (`[profile] contribute` / `consume`), matching the server's reading.
pub(crate) fn is_falsy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_marker(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join(".ai-memory.toml");
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn find_marker_walks_up_from_cwd() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let marker = write_marker(&root, "workspace = \"acme\"\n");
        let nested = root.join("a").join("b");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            find_marker_with_home(nested.to_str().unwrap(), Some(&root)),
            Some(marker),
            "a marker in any ancestor wins"
        );
    }

    /// The memoized lookup answers exactly as `find_marker` does from every
    /// directory, inside home and outside it, in and out of a checkout, with
    /// and without a home; each directory costs one probe of each kind at
    /// most, however many lookups pass it, and directories an earlier walk
    /// already covered (`remember`) cost none.
    #[test]
    fn marker_memo_answers_like_find_marker_once_per_directory() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        let dirs = [
            home.join("p").join("q").join("r"),
            home.join("s").join("t"),
            root.join("out").join("repo").join("u").join("v"),
            root.join("out").join("repo").join("w").join("z"),
            root.join("out").join("norepo").join("x").join("y"),
        ];
        for dir in &dirs {
            fs::create_dir_all(dir).unwrap();
        }
        fs::create_dir_all(root.join("out").join("repo").join(".git")).unwrap();
        for marked in [
            home.join("p"),
            root.join("out").join("repo"),
            root.join("out").join("repo").join("w"),
            root.join("out").join("norepo").join("x"),
        ] {
            write_marker(&marked, "workspace = \"w\"\n");
        }
        let every: Vec<PathBuf> = dirs
            .iter()
            .flat_map(|dir| dir.ancestors().take_while(|a| a.starts_with(&root)))
            .map(Path::to_path_buf)
            .collect();
        for home in [Some(home.as_path()), None] {
            let mut memo = MarkerMemo::with_home(home);
            let mut probes = 0;
            for dir in &every {
                let found = memo.find_marker(dir, &mut || {
                    probes += 1;
                    Some(())
                });
                assert_eq!(
                    found,
                    Some(find_marker_with_home(dir.to_str().unwrap(), home)),
                    "{dir:?} with home {home:?}"
                );
            }
            let levels = every
                .iter()
                .map(|dir| dir.components().count())
                .sum::<usize>();
            assert!(
                probes <= 2 * every.len() + 2 * root.components().count(),
                "{probes}"
            );
            assert!(probes < levels, "{probes} probes, {levels} levels");
            // A refused probe stops the lookup.
            let mut fresh = MarkerMemo::with_home(home);
            assert_eq!(fresh.find_marker(&dirs[0], &mut || None), None);
            // What an earlier walk found costs nothing to look up again.
            for dir in &dirs {
                let mut seeded = MarkerMemo::with_home(home);
                if home.is_none() || !dir.starts_with(home.unwrap()) {
                    seeded.checkout_root(dir, &mut || Some(()));
                }
                let found = find_marker_with_home(dir.to_str().unwrap(), home);
                seeded.remember(dir, found.as_deref());
                for ancestor in dir.ancestors().take_while(|a| a.starts_with(&root)) {
                    let mut spent = false;
                    let answer = seeded.find_marker(ancestor, &mut || {
                        spent = true;
                        Some(())
                    });
                    assert_eq!(
                        answer,
                        Some(find_marker_with_home(ancestor.to_str().unwrap(), home)),
                        "{ancestor:?} below {dir:?} with home {home:?}"
                    );
                    if ancestor == dir.as_path() {
                        assert!(!spent, "{dir:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn find_marker_returns_none_without_one() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(find_marker(tmp.path().to_str().unwrap()), None);
    }

    #[test]
    fn marker_walk_outside_home_stops_at_checkout_root() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let outer = tmp.path().join("outside");
        let repo = outer.join("repo");
        let nested = repo.join("src");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&home).unwrap();
        let outer_marker = write_marker(&outer, "workspace = \"wrong\"\n");
        let repo_marker = write_marker(&repo, "workspace = \"right\"\n");

        assert_eq!(
            find_marker_with_home(nested.to_str().unwrap(), Some(&home)),
            Some(repo_marker.clone())
        );
        fs::remove_file(repo.join(".ai-memory.toml")).unwrap();
        assert_eq!(
            find_marker_with_home(nested.to_str().unwrap(), Some(&home)),
            None,
            "a marker above the checkout boundary must not leak in"
        );
        assert!(outer_marker.exists());
    }

    /// A `$HOME` ending in a separator is the same boundary. The hook scripts
    /// mirror this.
    #[test]
    fn a_trailing_separator_on_home_keeps_the_boundary() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let repo = home.join("org").join("repo");
        let plain = home.join("plain");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&plain).unwrap();
        let org_marker = write_marker(&home.join("org"), "workspace = \"org\"\n");
        write_marker(tmp.path(), "workspace = \"above\"\nserver = \"x\"\n");
        let slashed = PathBuf::from(format!("{}/", home.display()));

        assert_eq!(
            find_marker_with_home(repo.to_str().unwrap(), Some(&slashed)),
            Some(org_marker)
        );
        assert_eq!(
            find_marker_with_home(plain.to_str().unwrap(), Some(&slashed)),
            None,
            "a marker above home must not leak in"
        );
        assert!(find_server_selection(plain.to_str().unwrap(), Some(&slashed)).is_none());
    }

    #[test]
    fn marker_walk_outside_home_checks_only_cwd_without_a_checkout() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let outer = tmp.path().join("outside");
        let cwd = outer.join("plain");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        write_marker(&outer, "workspace = \"wrong\"\n");

        assert_eq!(
            find_marker_with_home(cwd.to_str().unwrap(), Some(&home)),
            None
        );
        let local = write_marker(&cwd, "workspace = \"right\"\n");
        assert_eq!(
            find_marker_with_home(cwd.to_str().unwrap(), Some(&home)),
            Some(local)
        );
    }

    /// Happy-path TOML parser: extracts each declared root-level
    /// `key = "value"` pair. Mirrors the shell `ai_memory_parse_toml_key`.
    #[test]
    fn marker_aliases_match_the_shared_fixture() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../../ai-memory-core/fixtures/remote_identity_cases.json"
        ))
        .unwrap();
        for case in cases["marker_aliases"].as_array().unwrap() {
            let parsed = parse_aliases_in(case["toml"].as_str().unwrap());
            match case["status"].as_str().unwrap() {
                "valid" => {
                    let got = parsed.unwrap().unwrap().as_slice().to_vec();
                    let expected = case["aliases"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|value| value.as_str().unwrap().to_owned())
                        .collect::<Vec<_>>();
                    assert_eq!(got, expected, "{}", case["toml"]);
                }
                "absent" => assert_eq!(parsed.unwrap(), None, "{}", case["toml"]),
                "invalid" => assert!(parsed.is_err(), "{}", case["toml"]),
                status => panic!("unknown fixture status {status}"),
            }
        }
    }

    #[test]
    fn home_routes_match_the_shared_fixture() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../ai-memory-core/fixtures/home_route_cases.json"
        ))
        .unwrap();
        let home = Path::new("/home/operator");
        for case in fixture["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let generated = case["generate"].as_str();
            let text = match generated {
                Some("65_routes") => (0..65)
                    .map(|index| format!("[routes.path.\"/route/{index}\"]\nroute_workspace=\"ws\"\nroute_project=\"p{index}\"\n"))
                    .collect(),
                Some("selector_512") => format!(
                    "[routes.path.\"/{}\"]\nroute_workspace=\"bounds\"\nroute_project=\"valid\"\n",
                    "a".repeat(511)
                ),
                Some("selector_513") => format!(
                    "[routes.path.\"/{}\"]\nroute_workspace=\"bounds\"\nroute_project=\"invalid\"\n",
                    "a".repeat(512)
                ),
                Some("oversized_file") => format!("#{}\n", "x".repeat(MAX_HOME_ROUTE_FILE_BYTES)),
                Some(other) => panic!("unknown fixture generator {other}"),
                None => case["toml"]
                    .as_str()
                    .unwrap()
                    .replace("{{HOME}}", home.to_str().unwrap())
                    .replace("{{LONG_513}}", &"a".repeat(513)),
            };
            let parsed = HomeRoutes::parse(&text, home);
            if case["status"] == "invalid" {
                assert!(
                    parsed.is_err() || text.len() > MAX_HOME_ROUTE_FILE_BYTES,
                    "{name}"
                );
                continue;
            }
            let routes = parsed.unwrap();
            let cwd = match case["cwd_generate"].as_str() {
                Some("selector_512_child") => format!("/{}/child", "a".repeat(511)),
                Some(other) => panic!("unknown cwd fixture generator {other}"),
                None => case["cwd"]
                    .as_str()
                    .unwrap()
                    .replace("{{HOME}}", home.to_str().unwrap()),
            };
            let identity = case["identity"]
                .as_str()
                .map(|identity| RepositoryIdentity {
                    identity: identity.to_owned(),
                    source: IdentitySource::GitRemote,
                });
            let selected = routes.select(&cwd, identity.as_ref()).unwrap();
            if case["status"] == "none" {
                assert!(selected.is_none(), "{name}");
            } else {
                let (route, source) = selected.unwrap_or_else(|| panic!("{name}"));
                assert_eq!(
                    route.workspace,
                    case["workspace"].as_str().unwrap(),
                    "{name}"
                );
                assert_eq!(route.project, case["project"].as_str().unwrap(), "{name}");
                assert_eq!(
                    source,
                    if case["source"] == "identity" {
                        RoutingSource::HomeIdentityRoute
                    } else {
                        RoutingSource::HomePathRoute
                    },
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn home_routes_prefer_identity_then_longest_component_path() {
        let home = Path::new("/home/operator");
        let routes = HomeRoutes::parse(
            r#"
[routes.path."~/src"]
route_workspace = "path"
route_project = "broad"
[routes.path."~/src/api"]
route_workspace = "path"
route_project = "api"
[routes.identity."github.com/acme/api"]
route_workspace = "identity"
route_project = "acme-api"
route_identity_style = "path"
route_aliases = ["main"]
"#,
            home,
        )
        .unwrap();
        let identity = RepositoryIdentity {
            identity: "github.com/acme/api".into(),
            source: IdentitySource::GitRemote,
        };
        let (route, source) = routes
            .select("/home/operator/src/api/crate", Some(&identity))
            .unwrap()
            .unwrap();
        assert_eq!(source, RoutingSource::HomeIdentityRoute);
        assert_eq!(route.workspace, "identity");
        assert_eq!(route.project, "acme-api");
        assert_eq!(route.identity_style, Some(IdentityStyle::Path));
        assert_eq!(route.aliases.as_slice(), &["main"]);

        let (route, source) = routes
            .select("/home/operator/src/api/crate", None)
            .unwrap()
            .unwrap();
        assert_eq!(source, RoutingSource::HomePathRoute);
        assert_eq!(route.project, "api");
        assert!(
            routes
                .select("/home/operator/src/api-sibling", None)
                .unwrap()
                .is_some_and(|(route, _)| route.project == "broad")
        );
        assert!(
            routes
                .select("/home/operator/src-api", None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn home_routes_keep_forges_and_platform_path_roots_distinct() {
        let routes = HomeRoutes::parse(
            r#"
[routes.identity."github.com/acme/api"]
route_workspace = "oss"
route_project = "github-api"
[routes.identity."gitlab.com/acme/api"]
route_workspace = "oss"
route_project = "gitlab-api"
[routes.path."C:/Work/API"]
route_workspace = "win"
route_project = "drive"
[routes.path."//Server/Share/API"]
route_workspace = "win"
route_project = "unc"
"#,
            Path::new("C:/Users/tester"),
        )
        .unwrap();
        for (identity, expected) in [
            ("github.com/acme/api", "github-api"),
            ("gitlab.com/acme/api", "gitlab-api"),
        ] {
            let remote = RepositoryIdentity {
                identity: identity.into(),
                source: IdentitySource::GitRemote,
            };
            assert_eq!(
                routes
                    .select("C:/elsewhere", Some(&remote))
                    .unwrap()
                    .unwrap()
                    .0
                    .project,
                expected
            );
        }
        assert_eq!(
            routes
                .select(r"c:\work\api\src", None)
                .unwrap()
                .unwrap()
                .0
                .project,
            "drive"
        );
        assert_eq!(
            routes
                .select(r"\\server\share\api\src", None)
                .unwrap()
                .unwrap()
                .0
                .project,
            "unc"
        );
    }

    #[test]
    fn malformed_duplicate_and_ambiguous_home_routes_fail_closed() {
        let home = Path::new("/home/operator");
        for text in [
            "[routes.identity.\"api\"]\nroute_workspace=\"oss\"\nroute_project=\"api\"\n",
            "[routes.path.\"relative/api\"]\nroute_workspace=\"oss\"\nroute_project=\"api\"\n",
            "[routes.path.\"~/api\"]\nroute_workspace=\"Bad\"\nroute_project=\"api\"\n",
            "[routes.path.\"~/api\"]\nroute_workspace=\"oss\"\nroute_project=\"api\"\nroute_aliases=[\"../bad\"]\n",
            "[routes.path.\"~/api\"]\nroute_workspace=\"oss\"\nroute_project=\"api\"\nunknown=\"x\"\n",
        ] {
            assert!(HomeRoutes::parse(text, home).is_err(), "{text}");
        }
        let duplicate = "[routes.identity.\"github.com/acme/api\"]\nroute_workspace=\"oss\"\nroute_project=\"api\"\n[routes.identity.\"github.com/acme/api\"]\nroute_workspace=\"other\"\nroute_project=\"other\"\n";
        assert!(HomeRoutes::parse(duplicate, home).is_err());
        let escaped =
            "[routes.path.\"C:\\\\Work\\\\API\"]\nroute_workspace=\"oss\"\nroute_project=\"api\"\n";
        assert!(HomeRoutes::parse(escaped, home).is_err());
        let too_many = (0..=MAX_HOME_ROUTES)
            .map(|index| format!("[routes.path.\"/route/{index}\"]\nroute_workspace=\"oss\"\nroute_project=\"route-{index}\"\n"))
            .collect::<String>();
        assert!(HomeRoutes::parse(&too_many, home).is_err());

        let routes = HomeRoutes {
            identities: Vec::new(),
            paths: vec![
                PathRoute {
                    selector: PathSelector::parse("~/api", home).unwrap(),
                    route: HomeRoute {
                        workspace: "one".into(),
                        project: "one".into(),
                        identity_style: None,
                        aliases: MarkerAliases::default(),
                    },
                },
                PathRoute {
                    selector: PathSelector::parse("/home/operator/api", home).unwrap(),
                    route: HomeRoute {
                        workspace: "two".into(),
                        project: "two".into(),
                        identity_style: None,
                        aliases: MarkerAliases::default(),
                    },
                },
            ],
        };
        assert!(routes.select("/home/operator/api", None).is_err());
    }

    #[test]
    fn home_route_selection_works_outside_home_and_local_marker_wins() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let outside = tmp.path().join("outside/repo");
        fs::create_dir_all(outside.join(".git")).unwrap();
        fs::create_dir_all(&home).unwrap();
        write_marker(
            &home,
            &format!(
                "workspace = \"fallback\"\nproject = \"fallback\"\n[routes.path.\"{}\"]\nroute_workspace = \"routed\"\nroute_project = \"outside\"\n",
                outside.to_string_lossy().replace('\\', "/")
            ),
        );
        let selected = routing_selection_with_home(
            outside.to_str().unwrap(),
            outside.to_str().unwrap(),
            Some(&home),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(selected.source, RoutingSource::HomePathRoute);
        assert_eq!(selected.fields.workspace.as_deref(), Some("routed"));
        assert_eq!(selected.fields.project.as_deref(), Some("outside"));

        write_marker(&outside, "workspace = \"local\"\nproject = \"repo\"\n");
        let selected = routing_selection_with_home(
            outside.to_str().unwrap(),
            outside.to_str().unwrap(),
            Some(&home),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(selected.source, RoutingSource::LocalMarker);
        assert_eq!(selected.fields.workspace.as_deref(), Some("local"));
        assert_eq!(selected.fields.project.as_deref(), Some("repo"));
    }

    #[test]
    fn home_identity_route_forwards_aliases_only_with_remote_evidence() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&repo).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["remote", "add", "origin", "git@github.com:acme/api.git"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        write_marker(
            &home,
            "[routes.identity.\"github.com/acme/api\"]\nroute_workspace=\"oss\"\nroute_project=\"acme-api\"\nroute_identity_style=\"path\"\nroute_aliases=[\"main\"]\n",
        );
        let selected = routing_selection_with_home(
            repo.to_str().unwrap(),
            repo.to_str().unwrap(),
            Some(&home),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(selected.source, RoutingSource::HomeIdentityRoute);
        assert_eq!(selected.fields.project.as_deref(), Some("acme-api"));
        assert_eq!(selected.fields.identity_style.as_deref(), Some("path"));
        assert_eq!(
            selected.fields.aliases.unwrap().unwrap().as_slice(),
            &["main"]
        );
        assert_eq!(
            selected.remote_identity.unwrap().identity,
            "github.com/acme/api"
        );
    }

    #[test]
    fn home_root_settings_remain_the_fallback_without_a_route_match() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let checkout = tmp.path().join("outside/repo");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&checkout).unwrap();
        write_marker(
            &home,
            "workspace = \"fallback\"\nproject = \"fallback\"\n[routes.path.\"~/src\"]\nroute_workspace = \"oss\"\nroute_project = \"api\"\n",
        );
        let selected = routing_selection_with_home(
            checkout.to_str().unwrap(),
            checkout.to_str().unwrap(),
            Some(&home),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(selected.source, RoutingSource::HomeRoot);
        assert_eq!(selected.fields.workspace.as_deref(), Some("fallback"));
        assert_eq!(selected.fields.project.as_deref(), Some("fallback"));
    }

    #[test]
    fn matched_home_route_replaces_only_routing_fields() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let checkout = home.join("src/api/lib");
        fs::create_dir_all(&checkout).unwrap();
        write_marker(
            &home,
            "workspace=\"wrong\"\nproject=\"wrong\"\nproject_strategy=\"repo-root\"\ndrop_subagent_captures=\"true\"\n[recall]\ndefault_global=true\n[briefing]\ninject_on_session_start=true\nmax_chars=3210\n[profile]\ncontribute=true\nconsume=false\n[routes.path.\"~/src/api\"]\nroute_workspace=\"right\"\nroute_project=\"api\"\n",
        );
        let selected = routing_selection_with_home(
            checkout.to_str().unwrap(),
            checkout.to_str().unwrap(),
            Some(&home),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(selected.fields.workspace.as_deref(), Some("right"));
        assert_eq!(selected.fields.project.as_deref(), Some("api"));
        assert_eq!(selected.fields.project_strategy, None);
        let text = fs::read_to_string(selected.path).unwrap();
        assert_eq!(
            parse_key_in(&text, "drop_subagent_captures").as_deref(),
            Some("true")
        );
        assert_eq!(
            parse_flag_in(&text, "default_global").as_deref(),
            Some("true")
        );
        assert_eq!(
            parse_flag_in(&text, "inject_on_session_start").as_deref(),
            Some("true")
        );
        assert_eq!(parse_flag_in(&text, "consume").as_deref(), Some("false"));
    }

    #[test]
    fn recognized_root_scalars_require_one_exact_quoted_value() {
        for line in [
            "workspace = \"wrong\" junk \"x\"",
            "workspace = \"wrong\" # comment",
            "workspace = \"wrong\"\"x\"",
            "workspace = wrong",
        ] {
            assert!(recognizable_malformed_root_line(line), "{line}");
        }
        assert!(!recognizable_malformed_root_line("workspace = \"valid\""));
    }

    #[test]
    fn malformed_home_routes_do_not_fall_through_to_root_home_scope() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let checkout = tmp.path().join("outside/repo");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&checkout).unwrap();
        write_marker(
            &home,
            "workspace = \"wrong\"\nproject = \"wrong\"\n[routes.path.\"relative\"]\nroute_workspace = \"oss\"\nroute_project = \"api\"\n",
        );
        assert!(
            routing_selection_with_home(
                checkout.to_str().unwrap(),
                checkout.to_str().unwrap(),
                Some(&home),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn old_root_parser_ignores_namespaced_route_fields() {
        let text = r#"
[routes.identity."github.com/acme/api"]
route_workspace = "oss"
route_project = "acme-api"
route_identity_style = "path"
route_aliases = ["main"]
"#;
        assert_eq!(parse_key_in(text, "workspace"), None);
        assert_eq!(parse_key_in(text, "project"), None);
        assert_eq!(parse_key_in(text, "identity_style"), None);
        assert_eq!(parse_aliases_in(text).unwrap(), None);
    }

    #[test]
    fn parse_toml_key_extracts_root_level_strings() {
        let tmp = TempDir::new().unwrap();
        let marker = write_marker(
            tmp.path(),
            r#"
workspace = "acme"
project = "infra"
project_strategy = "repo-root"
"#,
        );
        assert_eq!(
            parse_toml_key(&marker, "workspace").as_deref(),
            Some("acme")
        );
        assert_eq!(parse_toml_key(&marker, "project").as_deref(), Some("infra"));
        assert_eq!(
            parse_toml_key(&marker, "project_strategy").as_deref(),
            Some("repo-root")
        );
        assert_eq!(parse_toml_key(&marker, "absent"), None);
    }

    /// Shapes the naive parser deliberately doesn't handle (parity with
    /// the shell `_lib.sh` helper) — pin the contract so a future
    /// "robustify" refactor doesn't silently start matching them.
    #[test]
    fn parse_toml_key_skips_unsupported_shapes() {
        let tmp = TempDir::new().unwrap();
        let marker = write_marker(
            tmp.path(),
            r#"
# Single-quoted values are not honoured.
workspace = 'acme'
# Comments after the value are not stripped.
project = "infra" # this is fine
"#,
        );
        assert_eq!(parse_toml_key(&marker, "workspace"), None);
        // The trailing comment is appended to the value because the parser
        // looks for the first `"` — pin it so the contract is explicit.
        assert_eq!(parse_toml_key(&marker, "project").as_deref(), Some("infra"));
    }

    #[test]
    fn parse_toml_flag_accepts_bare_and_quoted_tokens() {
        let tmp = TempDir::new().unwrap();
        let marker = write_marker(
            tmp.path(),
            "[recall]\ndefault_global = true\nmax_chars = 4000 # budget\n",
        );

        assert_eq!(
            parse_toml_flag(&marker, "default_global").as_deref(),
            Some("true")
        );
        assert_eq!(
            parse_toml_flag(&marker, "max_chars").as_deref(),
            Some("4000"),
            "trailing comments are stripped"
        );
    }

    #[test]
    fn is_truthy_matches_shell_parity_tokens() {
        for value in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(is_truthy(value), "{value} should be truthy");
        }
        for value in ["0", "false", "", "maybe"] {
            assert!(!is_truthy(value), "{value} should not be truthy");
        }
    }

    #[test]
    fn scope_declares_repo_root_for_both_spellings() {
        let base = MarkerScope {
            path: PathBuf::from("/tmp/.ai-memory.toml"),
            workspace: None,
            project: None,
            project_strategy: None,
            identity: None,
            identity_style: None,
            remote_identity: None,
        };

        for spelling in ["repo-root", "repo_root"] {
            let scope = MarkerScope {
                project_strategy: Some(spelling.to_string()),
                ..base.clone()
            };
            assert!(scope.is_repo_root(), "{spelling} should select repo-root");
            assert!(
                scope.declares_scope(),
                "{spelling} alone still changes resolution"
            );
        }

        assert!(
            !base.declares_scope(),
            "a marker with only [capture] rules must not change scope"
        );
    }

    // ── #668: a capture-only marker is scope/settings-transparent ────────

    /// A nested marker whose only content is `[capture]` must not shadow an
    /// outer ancestor's declared scope: `read_scope` walks past it and
    /// returns the OUTER marker's workspace/project.
    #[test]
    fn read_scope_skips_a_nested_capture_only_marker() {
        let tmp = TempDir::new().unwrap();
        let outer_marker = write_marker(tmp.path(), "workspace = \"acme\"\nproject = \"infra\"\n");
        let inner = tmp.path().join("sub");
        fs::create_dir_all(&inner).unwrap();
        write_marker(&inner, "[capture]\nignore_paths = [\"secret/**\"]\n");

        let scope = read_scope(inner.to_str().unwrap(), &RuntimeEnv::default())
            .unwrap()
            .expect("the outer marker still declares scope");
        assert_eq!(scope.workspace.as_deref(), Some("acme"));
        assert_eq!(scope.project.as_deref(), Some("infra"));
        // `scope.path` is the marker found by the lexical (non-canonicalizing)
        // walk, so it stays in the input's namespace — compare against the raw
        // marker path, not `canonicalize()` (which would diverge on macOS's
        // /var -> /private/var symlink and Windows's \\?\ prefix).
        assert_eq!(scope.path, outer_marker);
    }

    /// When every marker in the ancestor chain is capture-only (or none
    /// exist), `read_scope` returns `None` so the caller uses the normal
    /// remote-path identity, or the cwd basename when no valid remote exists.
    #[test]
    fn read_scope_still_none_when_only_capture_only_markers_exist() {
        let tmp = TempDir::new().unwrap();
        write_marker(tmp.path(), "[capture]\nignore_paths = [\"secret/**\"]\n");
        let inner = tmp.path().join("sub");
        fs::create_dir_all(&inner).unwrap();
        write_marker(&inner, "[capture]\nignore_paths = [\"other/**\"]\n");

        assert_eq!(
            read_scope(inner.to_str().unwrap(), &RuntimeEnv::default()).unwrap(),
            None
        );
    }

    /// A marker that declares `[briefing]` but no `workspace`/`project` is
    /// NOT capture-only — it declares a forwarded setting, so it is a
    /// resolution boundary. `read_scope` must not walk past it to an outer
    /// marker's scope, even though that marker declares one: behavior for
    /// this shape is exactly what it was before #668.
    #[test]
    fn read_scope_treats_a_briefing_only_marker_as_a_settings_boundary() {
        let tmp = TempDir::new().unwrap();
        write_marker(tmp.path(), "workspace = \"acme\"\nproject = \"infra\"\n");
        let inner = tmp.path().join("sub");
        fs::create_dir_all(&inner).unwrap();
        write_marker(&inner, "[briefing]\ninject_on_session_start = true\n");

        assert_eq!(
            read_scope(inner.to_str().unwrap(), &RuntimeEnv::default()).unwrap(),
            None,
            "a briefing-only marker is a settings boundary: it stops the walk \
             but declares no scope of its own"
        );
    }

    #[test]
    fn strict_inspection_rejects_invalid_routing_without_exposing_fields() {
        let tmp = TempDir::new().unwrap();
        let inner = tmp.path().join("sub");
        fs::create_dir_all(&inner).unwrap();
        write_marker(
            tmp.path(),
            "workspace = [\nproject = \"private-project\"\nidentity = \"private/identity\"\n",
        );

        let inspection = inspect_scope(inner.to_str().unwrap(), &RuntimeEnv::default());
        assert_eq!(inspection.status, "invalid");
        assert!(inspection.scope.is_none());
        assert!(inspection.fields.workspace.is_none());
        assert!(inspection.fields.project.is_none());
        assert!(inspection.fields.identity.is_none());
    }

    #[test]
    fn strict_inspection_uses_the_same_capture_only_walk_as_scope_resolution() {
        let tmp = TempDir::new().unwrap();
        write_marker(
            tmp.path(),
            "workspace = \"acme\"\nproject = \"infra\"\nidentity_style = \"path\"\n",
        );
        let inner = tmp.path().join("sub");
        fs::create_dir_all(&inner).unwrap();
        write_marker(&inner, "[capture]\nignore_paths = [\"secret/**\"]\n");

        let inspection = inspect_scope(inner.to_str().unwrap(), &RuntimeEnv::default());
        assert_eq!(inspection.status, "valid");
        assert_eq!(inspection.fields.workspace.as_deref(), Some("acme"));
        assert_eq!(inspection.fields.project.as_deref(), Some("infra"));
        assert_eq!(
            inspection.scope,
            read_scope(inner.to_str().unwrap(), &RuntimeEnv::default()).unwrap()
        );
    }

    #[test]
    fn declares_more_than_capture_is_conservative() {
        assert!(!declares_more_than_capture(
            "[capture]\nignore_paths = [\"a/**\"]\n"
        ));
        assert!(!declares_more_than_capture(""));
        for text in [
            "workspace = \"acme\"\n",
            "project = \"infra\"\n",
            "project_strategy = \"repo-root\"\n",
            "drop_subagent_captures = \"true\"\n",
            "[recall]\ndefault_global = true\n",
            "[briefing]\ninject_on_session_start = true\n",
            "[briefing]\ninject_on_subagent_start = true\n",
            "[briefing]\nmax_chars = 4000\n",
            "[handoff]\ndeliver_on_session_start = false\n",
        ] {
            assert!(
                declares_more_than_capture(text),
                "{text} should be a settings boundary"
            );
        }
    }

    // ── #671: `absolute_normalized` must be lexical, not filesystem-real ──

    /// A `..` component is resolved purely syntactically: it must not
    /// require the path to exist, which `fs::canonicalize` would (this path
    /// is guaranteed absent). Pins the regression that made macOS's
    /// `/private/var` symlink resolution and Windows' `\\?\` prefix diverge
    /// from the raw hook-reported candidate paths.
    // Unix-only fixtures (hardcoded `/`-rooted absolute paths); the lexical
    // fold itself is platform-agnostic and exercised on Windows by the capture
    // tests once they compile.
    #[cfg(unix)]
    #[test]
    fn absolute_normalized_resolves_dotdot_without_requiring_the_path_to_exist() {
        let missing = Path::new("/definitely/does/not/exist-671/nested/../sibling");
        assert_eq!(
            absolute_normalized(missing),
            PathBuf::from("/definitely/does/not/exist-671/sibling"),
            "`..` must resolve lexically even though the path is absent \
             (fs::canonicalize would have returned Err for this)"
        );
    }

    /// A leading `..` with nothing left to pop stays literal — lexical
    /// normalization can't escape above a root, matching what
    /// `fs::canonicalize` does for `/`.
    #[cfg(unix)]
    #[test]
    fn absolute_normalized_keeps_dotdot_that_cannot_go_above_root() {
        assert_eq!(
            absolute_normalized(Path::new("/../above-root")),
            PathBuf::from("/above-root")
        );
    }

    /// The regression itself: a REAL symlinked directory must be returned
    /// as-is, with the symlink component intact, rather than resolved to
    /// its target — the behavior `fs::canonicalize` had and that diverged
    /// `marker_dir` from the runtime hook's un-canonicalized candidate
    /// paths on macOS/Windows (`ignore_paths` silently stopped matching).
    // Unix-only: creating a symlink on Windows CI needs elevated privilege.
    // The regression this guards (canonicalize resolving the symlink and
    // diverging `marker_dir` from raw runtime paths) is exercised on Linux/macOS.
    #[cfg(unix)]
    #[test]
    fn absolute_normalized_does_not_resolve_a_real_symlink() {
        let tmp = TempDir::new().unwrap();
        let real_target = tmp.path().join("real-target");
        fs::create_dir_all(&real_target).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_target, &link).unwrap();

        let via_symlink = link.join("nested").join("..").join("file.txt");
        let normalized = absolute_normalized(&via_symlink);

        assert!(
            normalized.starts_with(&link),
            "normalized path {normalized:?} must keep the `link` component \
             rather than resolving it to {real_target:?}"
        );
        assert_eq!(normalized, link.join("file.txt"));
    }

    // ── #992: `server` profile selection ─────────────────────────────────

    #[test]
    fn server_selection_parses_quoted_bare_and_empty_values() {
        assert_eq!(
            server_selection_in("server = \"team-b\" # comment\n").as_deref(),
            Some("team-b")
        );
        assert_eq!(
            server_selection_in("  server=team-b # comment\n").as_deref(),
            Some("team-b")
        );
        assert_eq!(server_selection_in("server = \"\"\n").as_deref(), Some(""));
        assert_eq!(
            server_selection_in("servers = \"x\"\nserver_url = \"y\"\n"),
            None
        );
        assert_eq!(server_selection_in("workspace = \"a\"\n"), None);
    }

    /// A marker declaring only `server` is a settings boundary like any other
    /// root-level key; a `[capture]`-only marker stays transparent.
    #[test]
    fn a_server_only_marker_is_a_settings_boundary() {
        assert!(declares_more_than_capture("server = \"team-b\"\n"));
        assert!(declares_more_than_capture("server = team-b\n"));
        assert!(!declares_more_than_capture(
            "[capture]\nignore_paths = [\"x/**\"]\n"
        ));
    }

    /// Routing is inherited: a nested marker that sets only `workspace`, or
    /// only `[capture]`, keeps the ancestor's profile. Without this, a
    /// sub-project marker would silently send a profile-routed tree to the
    /// install-default server.
    #[test]
    fn nested_markers_without_server_inherit_the_ancestor_selection() {
        let tmp = TempDir::new().unwrap();
        write_marker(tmp.path(), "workspace = \"team-b\"\nserver = \"team-b\"\n");
        let scoped = tmp.path().join("scoped");
        let capture_only = scoped.join("capture-only");
        fs::create_dir_all(&capture_only).unwrap();
        write_marker(&scoped, "workspace = \"other\"\n");
        write_marker(&capture_only, "[capture]\nignore_paths = [\"x/**\"]\n");

        let selection = find_server_selection(capture_only.to_str().unwrap(), Some(tmp.path()))
            .expect("the ancestor's selection applies");
        assert_eq!(selection.name.as_deref(), Some("team-b"));
        assert_eq!(selection.marker_dir, absolute_normalized(tmp.path()));
    }

    #[test]
    fn the_nearest_server_declaration_wins() {
        let tmp = TempDir::new().unwrap();
        write_marker(tmp.path(), "server = \"team-a\"\n");
        let inner = tmp.path().join("inner");
        fs::create_dir_all(&inner).unwrap();
        write_marker(&inner, "server = \"team-b\"\n");

        let selection = find_server_selection(inner.to_str().unwrap(), Some(tmp.path())).unwrap();
        assert_eq!(selection.name.as_deref(), Some("team-b"));
        assert_eq!(selection.marker_dir, absolute_normalized(&inner));
    }

    #[test]
    fn no_server_key_anywhere_is_no_selection() {
        let tmp = TempDir::new().unwrap();
        write_marker(tmp.path(), "workspace = \"a\"\n");
        assert_eq!(
            find_server_selection(tmp.path().to_str().unwrap(), Some(tmp.path())),
            None
        );
    }

    /// Outside `$HOME`, an organisation-level marker above the repository's
    /// own `.git` still routes it; stopping at the checkout root would send
    /// the repository to the install default.
    #[test]
    fn outside_home_the_walk_reaches_a_marker_above_the_checkout_root() {
        let tmp = TempDir::new().unwrap();
        let org = tmp.path().join("org");
        let repo = org.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        write_marker(&org, "server = \"team-b\"\n");
        write_marker(&repo, "workspace = \"api\"\n");
        let elsewhere = tmp.path().join("home");

        let selection = find_server_selection(repo.to_str().unwrap(), Some(&elsewhere)).unwrap();
        assert_eq!(selection.name.as_deref(), Some("team-b"));
        assert_eq!(selection.marker_dir, absolute_normalized(&org));
    }

    /// A BOM or a non-UTF-8 byte must not hide the key.
    /// The same BOM that could hide a first-line `server` key could hide a
    /// first-line scope key or flag: a marker saved as "UTF-8 with BOM"
    /// routed capture to the fallback workspace.
    #[test]
    fn a_bom_cannot_hide_a_first_line_scope_key_or_flag() {
        let routing = RoutingFields::from_text("\u{feff}workspace = \"acme\"\nproject = \"api\"\n");
        assert_eq!(routing.workspace.as_deref(), Some("acme"));
        assert_eq!(routing.project.as_deref(), Some("api"));
        assert_eq!(
            parse_flag_in("\u{feff}default_global = true\n", "default_global").as_deref(),
            Some("true")
        );
    }

    #[test]
    fn encoding_noise_cannot_hide_a_server_key() {
        let tmp = TempDir::new().unwrap();
        for bytes in [
            b"\xEF\xBB\xBFserver = \"team-b\"\n".as_slice(),
            b"# caf\xE9\nserver = \"team-b\"\n".as_slice(),
        ] {
            fs::write(tmp.path().join(".ai-memory.toml"), bytes).unwrap();
            let selection =
                find_server_selection(tmp.path().to_str().unwrap(), Some(tmp.path())).unwrap();
            assert_eq!(selection.name.as_deref(), Some("team-b"), "{bytes:?}");
        }
    }

    /// A marker that exists but cannot be read is a refused selection, not
    /// "no selection" — it may well declare a profile.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_marker_is_a_refused_selection() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = TempDir::new().unwrap();
        let marker = write_marker(tmp.path(), "server = \"team-b\"\n");
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&marker).is_ok() {
            // Running as root: permissions cannot make the file unreadable.
            return;
        }
        let selection =
            find_server_selection(tmp.path().to_str().unwrap(), Some(tmp.path())).unwrap();
        assert_eq!(selection.name, None);
    }

    /// A profile's `roots` must admit a checkout whose drive-letter cwd
    /// differs from the registered root only in case — the spelling a
    /// harness reports on a case-insensitive filesystem. The selector logic
    /// is textual, so these assertions hold on every host.
    #[test]
    fn path_contains_folds_case_under_a_windows_root_only() {
        assert!(path_contains(
            Path::new(r"C:\Work\team-b"),
            Path::new(r"C:\work\team-b\repo")
        ));
        assert!(path_contains(
            Path::new(r"\\Server\Share\Work"),
            Path::new(r"\\server\share\work\repo")
        ));
        // Folding case must not also loosen the component boundary.
        assert!(!path_contains(
            Path::new(r"C:\Work\team-b"),
            Path::new(r"C:\work\team-bb\repo")
        ));
        assert!(!path_contains(
            Path::new(r"C:\Work\team-b"),
            Path::new(r"D:\work\team-b\repo")
        ));
        // Control: a POSIX root stays case-sensitive.
        assert!(!path_contains(
            Path::new("/work/Team-B"),
            Path::new("/work/team-b/repo")
        ));
        assert!(path_contains(
            Path::new("/work/team-b"),
            Path::new("/work/team-b/repo")
        ));
    }
}
