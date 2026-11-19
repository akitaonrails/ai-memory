//! Path helpers shared by command renderers and hook capture.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Create `dir` (and its parents) `0700` on Unix. On Windows every newly
/// created directory component gets an access list naming only the current
/// account ([`restrict_to_owner`]), which everything later created inside it
/// inherits; if restricting a newly created directory fails, newly created
/// directories are removed so a retry never accepts a wide-ACL directory as
/// private. Idempotent: an existing directory's mode or access list is left
/// untouched (never widened, never narrowed) to avoid churning a path an
/// operator may have set deliberately.
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        if dir.is_dir() {
            return Ok(());
        }
        match std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
        {
            Ok(()) => Ok(()),
            // A concurrent caller may have created it between the check and
            // the call; treat an existing directory as success.
            Err(_) if dir.is_dir() => Ok(()),
            Err(e) => Err(e),
        }
    }
    #[cfg(windows)]
    {
        create_private_dir_with(dir, restrict_to_owner)
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Create every missing component of `dir` in order, calling `restrict` on
/// each newly created directory. If `create_dir` or `restrict` fails partway
/// through, best-effort remove the newly created directories in reverse order
/// so a failed ACL call cannot leave a wide-ACL directory on disk that a retry
/// would silently accept as private.
#[cfg(any(windows, test))]
fn create_private_dir_with(
    dir: &Path,
    mut restrict: impl FnMut(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }

    let mut missing = Vec::new();
    let mut current = Some(dir);
    while let Some(path) = current {
        if path.as_os_str().is_empty() || path.exists() {
            break;
        }
        missing.push(path.to_path_buf());
        current = path.parent();
    }
    if missing.is_empty() {
        // `dir` exists as a non-directory (e.g. a regular file); let
        // `create_dir` surface the platform error.
        return std::fs::create_dir(dir);
    }
    missing.reverse();

    let mut created = Vec::with_capacity(missing.len());
    for component in missing {
        match std::fs::create_dir(&component) {
            Ok(()) => {
                created.push(component.clone());
                if let Err(err) = restrict(&component) {
                    for dir_to_remove in created.into_iter().rev() {
                        let _ = std::fs::remove_dir(&dir_to_remove);
                    }
                    return Err(err);
                }
            }
            // A concurrent caller may have created this component between the
            // existence check and `create_dir`; leave its existing ACL alone.
            Err(_) if component.is_dir() => {}
            Err(err) => {
                for dir_to_remove in created.into_iter().rev() {
                    let _ = std::fs::remove_dir(&dir_to_remove);
                }
                return Err(err);
            }
        }
    }
    Ok(())
}

/// Resolve a Windows built-in tool (`icacls.exe`, `whoami.exe`) from
/// `%SystemRoot%\System32` so a Git for Windows / MSYS2 tool earlier on
/// `PATH` (notably `whoami.exe`) cannot shadow or hijack it. Falls back to
/// the bare command name only when `SystemRoot` is unset or empty.
#[cfg(any(windows, test))]
fn windows_system32_tool_in(system_root: Option<std::ffi::OsString>, exe_name: &str) -> PathBuf {
    if let Some(root) =
        system_root.filter(|v| !v.is_empty() && v.to_str().is_none_or(|t| !t.trim().is_empty()))
    {
        PathBuf::from(root).join("System32").join(exe_name)
    } else {
        PathBuf::from(exe_name.strip_suffix(".exe").unwrap_or(exe_name))
    }
}

#[cfg(windows)]
fn windows_system32_tool(exe_name: &str) -> PathBuf {
    windows_system32_tool_in(std::env::var_os("SystemRoot"), exe_name)
}

/// Normalize a path before passing it to `icacls.exe`: `icacls` treats `?` as
/// a wildcard and can reject `\\?\` verbatim prefixes produced by
/// `std::fs::canonicalize`. Safe drive and UNC verbatim prefixes are stripped
/// via [`strip_windows_verbatim_prefix`]; non-UTF-8 or unknown verbatim forms
/// are passed through unchanged.
#[cfg(any(windows, test))]
fn icacls_target_path(path: &Path) -> Cow<'_, Path> {
    let Some(text) = path.to_str() else {
        return Cow::Borrowed(path);
    };
    match strip_windows_verbatim_prefix(text) {
        Cow::Borrowed(stripped) if stripped.len() == text.len() => Cow::Borrowed(path),
        Cow::Borrowed(stripped) => Cow::Borrowed(Path::new(stripped)),
        Cow::Owned(stripped) => Cow::Owned(PathBuf::from(stripped)),
    }
}

/// Replace `path`'s inherited access list with one entry: full control for
/// the account this process runs as. A directory passes that entry on to
/// everything created inside it.
///
/// Windows has no mode bits for `0600`/`0700`. A new file or directory
/// inherits its parent's list, so a read grant anywhere above the data dir
/// (a sandbox group on `%LOCALAPPDATA%`, say) would otherwise reach a bearer
/// written there. Explicit entries an operator added to `path` itself are
/// kept. The account is named by SID, so the grant does not depend on the
/// display language or on how the account name resolves.
///
/// # Errors
/// The current account's SID cannot be read, or `icacls` fails; callers
/// treat that as a failed write rather than leave the secret readable.
#[cfg(windows)]
pub(crate) fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    let sid = current_account_sid()?;
    let grant = if path.is_dir() {
        format!("*{sid}:(OI)(CI)F")
    } else {
        format!("*{sid}:F")
    };
    let target = icacls_target_path(path);
    // Resolve `icacls.exe` explicitly under `%SystemRoot%\System32` so a
    // tool earlier on `PATH` cannot shadow or hijack it.
    let output = std::process::Command::new(windows_system32_tool("icacls.exe"))
        .arg(target.as_ref())
        .args(["/inheritance:r", "/grant:r"])
        .arg(grant)
        .arg("/q")
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "icacls could not restrict {} to the current account",
            path.display()
        )))
    }
}

/// The SID of the account this process runs as, from `whoami /user`.
#[cfg(windows)]
fn current_account_sid() -> std::io::Result<String> {
    // Resolve `whoami.exe` explicitly under `%SystemRoot%\System32` so a
    // Git for Windows / MSYS2 `whoami.exe` earlier on `PATH` cannot shadow it.
    let output = std::process::Command::new(windows_system32_tool("whoami.exe"))
        .args(["/user", "/fo", "csv", "/nh"])
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(
            "whoami could not report the current account",
        ));
    }
    parse_whoami_sid(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| std::io::Error::other("whoami reported no SID for the current account"))
}

/// The SID column of `whoami /user /fo csv /nh` (`"domain\user","S-1-5-..."`).
/// Only a well-formed SID comes back, so nothing else reaches `icacls`.
#[cfg(any(windows, test))]
fn parse_whoami_sid(csv: &str) -> Option<String> {
    let sid = csv
        .lines()
        .next()?
        .rsplit(',')
        .next()?
        .trim()
        .trim_matches('"');
    let valid = sid.starts_with("S-1-")
        && sid.len() > 4
        && sid[4..]
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    valid.then(|| sid.to_owned())
}

/// Read a path's access list in a form tests can check without depending on
/// the display language: the SDDL `icacls /save` writes.
#[cfg(all(test, windows))]
pub(crate) mod acl_probe {
    use std::path::Path;

    /// The SIDs (or SDDL aliases such as `BU`) of every allow entry on `path`.
    pub(crate) fn allowed_sids(path: &Path) -> Vec<String> {
        let scratch = tempfile::tempdir().unwrap();
        let saved = scratch.path().join("acl.txt");
        let target = super::icacls_target_path(path);
        let saved_target = super::icacls_target_path(&saved);
        let output = std::process::Command::new(super::windows_system32_tool("icacls.exe"))
            .arg(target.as_ref())
            .arg("/save")
            .arg(saved_target.as_ref())
            .arg("/q")
            .output()
            .unwrap();
        assert!(output.status.success(), "icacls /save failed: {output:?}");
        let raw = std::fs::read(&saved).unwrap();
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let text = String::from_utf16_lossy(&units);
        let sddl = text.lines().nth(1).unwrap_or_default().to_owned();
        let dacl = sddl.split("D:").nth(1).unwrap_or_default();
        let dacl = dacl.split("S:").next().unwrap_or_default();
        dacl.split('(')
            .skip(1)
            .filter_map(|ace| {
                let fields: Vec<&str> = ace.trim_end_matches(')').split(';').collect();
                (fields.first() == Some(&"A")).then(|| fields.last().unwrap().to_string())
            })
            .collect()
    }

    /// The current account's SID.
    pub(crate) fn current_sid() -> String {
        super::current_account_sid().unwrap()
    }

    /// Give the built-in Users group read access to `dir` and everything
    /// created inside it, the way a sandbox group's grant on
    /// `%LOCALAPPDATA%` reaches the data dir.
    pub(crate) fn grant_users_read(dir: &Path) {
        let target = super::icacls_target_path(dir);
        let output = std::process::Command::new(super::windows_system32_tool("icacls.exe"))
            .arg(target.as_ref())
            .args(["/grant", "*S-1-5-32-545:(OI)(CI)R", "/q"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    /// Whether an allow entry names the built-in Users group.
    pub(crate) fn names_users(sids: &[String]) -> bool {
        sids.iter().any(|sid| sid == "BU" || sid == "S-1-5-32-545")
    }
}

/// The working directory as the shell names it, when that is the same
/// directory as `physical`.
///
/// `std::env::current_dir` returns the physical path (`getcwd` resolves
/// symlinks: macOS `/var/...` comes back as `/private/var/...`), while a
/// harness reports the logical path its shell keeps in `PWD`, and hook
/// routing compares that one lexically (#2016). Paths are never
/// canonicalized for that comparison (#671), so a command that must agree
/// with the hooks takes `pwd` instead, but only when it is absolute, has no
/// `.`/`..` component, and opens the very directory `physical` does (same
/// device and inode). A stale or foreign `PWD`, or one that cannot be
/// checked, leaves `physical`, so a refusal stays a refusal. Windows keeps
/// the path a process was started in, so it has no such split and keeps
/// `physical`.
pub(crate) fn logical_dir(physical: &Path, pwd: Option<&std::ffi::OsStr>) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        use std::path::Component;
        let Some(pwd) = pwd.map(Path::new) else {
            return physical.to_path_buf();
        };
        let plain = pwd.is_absolute()
            && pwd
                .components()
                .all(|part| matches!(part, Component::RootDir | Component::Normal(_)));
        let same = |a: &Path, b: &Path| match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => a.is_dir() && a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        };
        if plain && same(pwd, physical) {
            return pwd.to_path_buf();
        }
        physical.to_path_buf()
    }
    #[cfg(not(unix))]
    {
        let _ = pwd;
        physical.to_path_buf()
    }
}

/// Resolve the user home used for agent configuration paths.
///
/// Tests and scripted wrappers can set `AI_MEMORY_HOME` to exercise the
/// platform-specific config layout without touching the real user profile.
pub(crate) fn home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("AI_MEMORY_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    dirs::home_dir()
}

/// An agent's relocated config root from its own home variable, when that
/// variable is set to a non-empty, non-whitespace value; else `None`, and the
/// caller falls back to the agent's `~/...` default.
///
/// Blank is treated as unset on purpose: an exported-but-empty variable is far
/// more often an unset shell expansion than a deliberate request to install
/// into the filesystem root. The rule lives in
/// [`ai_memory_workstream::env_dir_override`] so native session import applies
/// the same one and never reads a store the installers did not wire.
///
/// The env value comes in as a parameter so tests can exercise both branches
/// without mutating process env — which is not merely inconvenient here but
/// forbidden: `std::env::set_var` is `unsafe` under edition 2024 and this
/// workspace forbids `unsafe_code`.
pub(crate) fn agent_config_home(env_override: Option<std::ffi::OsString>) -> Option<PathBuf> {
    ai_memory_workstream::env_dir_override(env_override)
}

/// Hermes Agent's home, as Hermes resolves it: `$HERMES_HOME` when set,
/// else its platform default (`get_default_hermes_root` and
/// `_platform_default_hermes_home` in `hermes_constants.py`).
///
/// The default is `%LOCALAPPDATA%\hermes` on Windows and `~/.hermes`
/// elsewhere, with `$HERMES_DATA_DIR_SUFFIX` appended to the name. The
/// Windows default is not guesswork: the Windows installer puts the home
/// under `%LOCALAPPDATA%`, and the launcher exports `HERMES_HOME` only into
/// its own process tree, so a command that runs outside Hermes (doctor,
/// install-skills) found no home at all when it looked at `~/.hermes`.
pub(crate) fn hermes_home(env: &crate::config::RuntimeEnv, home: Option<&Path>) -> Option<PathBuf> {
    hermes_home_in(
        env.var_os("HERMES_HOME"),
        env.var_os("LOCALAPPDATA"),
        env.var_os("HERMES_DATA_DIR_SUFFIX"),
        home,
        cfg!(windows),
    )
}

/// [`hermes_home`] with the environment passed in, so tests can exercise the
/// Windows and POSIX defaults on either host.
pub(crate) fn hermes_home_in(
    env_home: Option<std::ffi::OsString>,
    local_appdata: Option<std::ffi::OsString>,
    data_dir_suffix: Option<std::ffi::OsString>,
    home: Option<&Path>,
    windows: bool,
) -> Option<PathBuf> {
    if let Some(home) = agent_config_home(env_home) {
        return Some(home);
    }
    let suffix = data_dir_suffix
        .and_then(|value| value.into_string().ok())
        .unwrap_or_default();
    if windows {
        // Hermes reads `%LOCALAPPDATA%` and falls back to the user profile's
        // `AppData\Local` when it is empty.
        let base = local_appdata
            .filter(|value| value.to_str().is_none_or(|text| !text.trim().is_empty()))
            .map(PathBuf::from)
            .or_else(|| home.map(|home| home.join("AppData").join("Local")))?;
        return Some(base.join(format!("hermes{suffix}")));
    }
    home.map(|home| home.join(format!(".hermes{suffix}")))
}

/// Devin CLI's user-level config root: `%APPDATA%\devin` on Windows (falling
/// back to `<home>/.config/devin` when `APPDATA` is unset or empty), and
/// `<home>/.config/devin` on Linux/macOS/WSL.
pub(crate) fn devin_user_config_dir() -> Option<PathBuf> {
    let home = home_dir();
    devin_user_config_dir_in(std::env::var_os("APPDATA"), home.as_deref(), cfg!(windows))
}

/// [`devin_user_config_dir`] with the environment passed in, so tests can
/// exercise both Windows and POSIX defaults on either host.
pub(crate) fn devin_user_config_dir_in(
    appdata: Option<std::ffi::OsString>,
    home: Option<&Path>,
    windows: bool,
) -> Option<PathBuf> {
    if windows
        && let Some(base) = appdata
            .filter(|value| value.to_str().is_none_or(|text| !text.trim().is_empty()))
            .map(PathBuf::from)
    {
        return Some(base.join("devin"));
    }
    home.map(|home| home.join(".config").join("devin"))
}

/// Hermes profile ids follow `^[a-z0-9][a-z0-9_-]{0,63}$`.
pub(crate) fn is_hermes_profile_id(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() <= 64
        && bytes
            .first()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

/// Claude Code's relocated config root: `$CLAUDE_CONFIG_DIR` when set, else
/// `None`. Named alias kept so Claude call sites read for what they resolve.
pub(crate) fn claude_config_dir(env_override: Option<std::ffi::OsString>) -> Option<PathBuf> {
    agent_config_home(env_override)
}

/// Candidate Claude Code paths for uninstall. The active relocated path comes
/// first, followed by the legacy home path, with exact duplicates removed.
pub(crate) fn claude_config_paths(
    home: Option<&Path>,
    relocated: Option<&Path>,
    legacy_relative: &Path,
    relocated_relative: &Path,
) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(2);
    if let Some(root) = relocated {
        paths.push(root.join(relocated_relative));
    }
    if let Some(root) = home {
        let legacy = root.join(legacy_relative);
        if !paths.contains(&legacy) {
            paths.push(legacy);
        }
    }
    paths
}

/// `path` as shown to the operator: without a safe `\\?\` verbatim prefix,
/// which the canonical data dir carries on Windows and which `cmd` and most
/// scripts cannot use when the path is copied out of a message.
pub(crate) fn display_path(path: &Path) -> String {
    strip_windows_verbatim_prefix(&path.to_string_lossy())
        .chars()
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .collect()
}

/// Strip only Windows verbatim path prefixes that are safe to render as plain
/// paths. Unknown verbatim forms are left unchanged.
pub(crate) fn strip_windows_verbatim_prefix(path: &str) -> Cow<'_, str> {
    let Some(rest) = path.strip_prefix(r"\\?\") else {
        return Cow::Borrowed(path);
    };

    if rest.len() >= 3
        && rest.as_bytes()[0].is_ascii_alphabetic()
        && rest.as_bytes()[1] == b':'
        && rest.as_bytes()[2] == b'\\'
    {
        return Cow::Borrowed(rest);
    }

    let bytes = rest.as_bytes();
    if bytes.len() >= 4
        && bytes[0].eq_ignore_ascii_case(&b'U')
        && bytes[1].eq_ignore_ascii_case(&b'N')
        && bytes[2].eq_ignore_ascii_case(&b'C')
        && bytes[3] == b'\\'
    {
        return Cow::Owned(format!(r"\\{}", &rest[4..]));
    }

    Cow::Borrowed(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    /// Only a well-formed SID reaches `icacls`; anything else from `whoami`
    /// is refused rather than passed on as an account to grant.
    #[test]
    fn parse_whoami_sid_takes_only_a_well_formed_sid() {
        assert_eq!(
            parse_whoami_sid("\"host\\user\",\"S-1-5-21-1-2-3-1001\"\r\n").as_deref(),
            Some("S-1-5-21-1-2-3-1001")
        );
        for bad in [
            "",
            "\"host\\user\"",
            "\"host\\user\",\"S-1-\"",
            "\"host\\user\",\"S-1-5-x\"",
            "\"host\\user\",\"Everyone:F\"",
            "\"host\\user\",\"S-1-5--1\"",
        ] {
            assert_eq!(parse_whoami_sid(bad), None, "{bad:?}");
        }
    }

    /// A read grant above a new private dir (a sandbox group on
    /// `%LOCALAPPDATA%`) must not reach the dir or a file later created in
    /// it; the control dir made the ordinary way shows the grant does reach
    /// a sibling. An existing dir is left as it was.
    #[cfg(windows)]
    #[test]
    fn a_new_private_dir_drops_a_read_grant_inherited_from_above() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        acl_probe::grant_users_read(&root);
        let control = root.join("plain");
        std::fs::create_dir_all(&control).unwrap();
        assert!(acl_probe::names_users(&acl_probe::allowed_sids(&control)));

        let private = root.join("private").join("nested");
        create_private_dir(&private).unwrap();
        let inside = private.join("secret.txt");
        std::fs::write(&inside, "s3cret").unwrap();

        let owner = acl_probe::current_sid();
        for path in [
            private.parent().unwrap(),
            private.as_path(),
            inside.as_path(),
        ] {
            let sids = acl_probe::allowed_sids(path);
            assert!(
                !sids.is_empty() && sids.iter().all(|sid| *sid == owner),
                "{} must be reachable by the owner alone, got {sids:?}",
                path.display()
            );
        }

        create_private_dir(&control).unwrap();
        assert!(
            acl_probe::names_users(&acl_probe::allowed_sids(&control)),
            "an existing directory's access list is never narrowed"
        );
    }

    /// `std::fs::canonicalize` on Windows produces a `\\?\` verbatim path,
    /// which is how `data_dir` reaches `create_private_dir` and
    /// `store_hook_auth_token` in real runs. `icacls.exe` must succeed and
    /// narrow the ACL on those canonicalized paths as well.
    #[cfg(windows)]
    #[test]
    fn canonicalized_verbatim_data_dir_is_restricted_by_icacls() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        acl_probe::grant_users_read(&data_dir);

        let canonical_data_dir = std::fs::canonicalize(&data_dir).unwrap();
        assert!(
            canonical_data_dir.to_string_lossy().starts_with(r"\\?\"),
            "expected Windows canonicalize to return a verbatim path, got {}",
            canonical_data_dir.display()
        );

        let private_dir = canonical_data_dir.join("private").join("nested");
        create_private_dir(&private_dir).unwrap();
        crate::config::store_hook_auth_token(&canonical_data_dir, "verbatim-bearer").unwrap();

        let owner = acl_probe::current_sid();
        for path in [
            private_dir.parent().unwrap().to_path_buf(),
            private_dir,
            crate::config::hook_auth_token_path_in(&canonical_data_dir),
            crate::config::hook_auth_header_path_in(&canonical_data_dir),
        ] {
            let sids = acl_probe::allowed_sids(&path);
            assert!(
                !sids.is_empty() && sids.iter().all(|sid| *sid == owner),
                "{} must be reachable by the owner alone, got {sids:?}",
                path.display()
            );
        }
        assert_eq!(
            crate::config::read_hook_auth_token(&canonical_data_dir).as_deref(),
            Some("verbatim-bearer")
        );
    }

    #[test]
    fn windows_system32_tool_in_resolves_under_system_root_and_falls_back_when_unset() {
        assert_eq!(
            windows_system32_tool_in(Some(OsString::from(r"C:\Windows")), "icacls.exe"),
            PathBuf::from(r"C:\Windows")
                .join("System32")
                .join("icacls.exe")
        );
        assert_eq!(
            windows_system32_tool_in(Some(OsString::from(r"C:\Windows")), "whoami.exe"),
            PathBuf::from(r"C:\Windows")
                .join("System32")
                .join("whoami.exe")
        );
        for unset in [None, Some(OsString::new()), Some(OsString::from("   "))] {
            assert_eq!(
                windows_system32_tool_in(unset.clone(), "icacls.exe"),
                PathBuf::from("icacls"),
                "{unset:?}"
            );
            assert_eq!(
                windows_system32_tool_in(unset.clone(), "whoami.exe"),
                PathBuf::from("whoami"),
                "{unset:?}"
            );
        }
    }

    #[test]
    fn icacls_target_path_strips_safe_verbatim_prefixes() {
        assert_eq!(
            icacls_target_path(Path::new(r"\\?\C:\Users\me\data\auth-token")).as_ref(),
            Path::new(r"C:\Users\me\data\auth-token")
        );
        assert_eq!(
            icacls_target_path(Path::new(r"\\?\UNC\server\share\data")).as_ref(),
            Path::new(r"\\server\share\data")
        );
        assert_eq!(
            icacls_target_path(Path::new(r"C:\Users\me\data")).as_ref(),
            Path::new(r"C:\Users\me\data")
        );
        assert_eq!(
            icacls_target_path(Path::new(
                r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\m"
            ))
            .as_ref(),
            Path::new(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\m")
        );
    }

    #[test]
    fn create_private_dir_with_restricts_every_new_component_and_rolls_back_on_error() {
        let tmp = tempfile::tempdir().unwrap();
        let existing = tmp.path().join("existing");
        std::fs::create_dir_all(&existing).unwrap();

        let target = existing.join("a").join("b").join("c");
        let mut restricted = Vec::new();
        create_private_dir_with(&target, |p| {
            restricted.push(p.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            restricted,
            vec![
                existing.join("a"),
                existing.join("a").join("b"),
                existing.join("a").join("b").join("c"),
            ]
        );
        assert!(target.is_dir());

        // Idempotent: an existing directory is left untouched.
        let mut called = false;
        create_private_dir_with(&target, |_| {
            called = true;
            Ok(())
        })
        .unwrap();
        assert!(!called, "existing directory must not be restricted again");

        // Rollback: if restricting a nested new directory fails, every newly
        // created component is removed so a retry never sees a wide-ACL dir.
        let failing = existing.join("x").join("y").join("z");
        let err = create_private_dir_with(&failing, |p| {
            if p == existing.join("x").join("y") {
                Err(std::io::Error::other("simulated icacls failure"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(err.to_string().contains("simulated icacls failure"));
        assert!(
            !existing.join("x").exists(),
            "failed restrict must roll back all newly created components"
        );
        assert!(
            existing.is_dir(),
            "pre-existing ancestor must not be removed"
        );

        // Collision with an existing regular file returns an error without
        // invoking restrict.
        let file = existing.join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        let mut file_restricted = false;
        assert!(
            create_private_dir_with(&file, |_| {
                file_restricted = true;
                Ok(())
            })
            .is_err()
        );
        assert!(!file_restricted);
    }

    /// #2016: `PWD` names the directory only when it opens the same one as
    /// the physical cwd; anything else keeps the physical path.
    #[cfg(unix)]
    #[test]
    fn logical_dir_takes_pwd_only_for_the_same_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real").join("repo");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(tmp.path().join("real"), &link).unwrap();
        let logical = link.join("repo");
        let pwd = |path: &Path| Some(path.as_os_str().to_owned());

        assert_eq!(logical_dir(&real, pwd(&logical).as_deref()), logical);
        assert_eq!(logical_dir(&real, pwd(&real).as_deref()), real);
        for refused in [
            pwd(&other),
            pwd(&link),
            pwd(&tmp.path().join("missing")),
            pwd(&link.join("repo").join("..").join("repo")),
            Some(OsString::from("link/repo")),
            None,
        ] {
            assert_eq!(logical_dir(&real, refused.as_deref()), real, "{refused:?}");
        }
        // A file with the directory's identity is impossible, but a plain file
        // never stands in for it either.
        let file = tmp.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert_eq!(logical_dir(&real, pwd(&file).as_deref()), real);
    }

    #[test]
    fn claude_config_dir_honours_non_empty_value() {
        assert_eq!(
            claude_config_dir(Some(OsString::from("/stores/claude"))),
            Some(PathBuf::from("/stores/claude"))
        );
    }

    #[test]
    fn claude_config_dir_treats_unset_empty_and_whitespace_as_unset() {
        for env in [None, Some(OsString::new()), Some(OsString::from("   "))] {
            assert_eq!(claude_config_dir(env.clone()), None, "env {env:?}");
        }
    }

    /// The Windows setup puts Hermes under `%LOCALAPPDATA%\hermes` and does
    /// not export `HERMES_HOME`, so the platform default is the only way an
    /// out-of-process command finds it; POSIX keeps `~/.hermes`.
    #[test]
    fn hermes_home_models_the_platform_default() {
        let home = Path::new("/home/alice");
        assert_eq!(
            hermes_home_in(None, None, None, Some(home), false),
            Some(home.join(".hermes"))
        );
        assert_eq!(
            hermes_home_in(None, None, None, Some(home), true),
            Some(home.join("AppData").join("Local").join("hermes"))
        );
        assert_eq!(
            hermes_home_in(
                None,
                Some(r"C:\Users\alice\AppData\Local".into()),
                None,
                Some(home),
                true
            ),
            Some(PathBuf::from(r"C:\Users\alice\AppData\Local").join("hermes"))
        );
        // Hermes appends the suffix to the default's name, never to
        // `$HERMES_HOME`.
        assert_eq!(
            hermes_home_in(None, None, Some("_dev".into()), Some(home), false),
            Some(home.join(".hermes_dev"))
        );
        assert_eq!(
            hermes_home_in(
                Some("/srv/hermes".into()),
                None,
                Some("_dev".into()),
                Some(home),
                true
            ),
            Some(PathBuf::from("/srv/hermes"))
        );
        // An exported-but-empty or whitespace value is an unset expansion.
        for unset in [None, Some(std::ffi::OsString::new()), Some("  ".into())] {
            assert_eq!(
                hermes_home_in(unset, None, None, Some(home), false),
                Some(home.join(".hermes"))
            );
        }
        assert_eq!(hermes_home_in(None, None, None, None, false), None);
        assert_eq!(hermes_home_in(None, None, None, None, true), None);
    }

    #[test]
    fn claude_config_paths_put_relocated_first_and_deduplicate() {
        assert_eq!(
            claude_config_paths(
                Some(Path::new("/home/alice")),
                Some(Path::new("/stores/claude")),
                Path::new(".claude/settings.json"),
                Path::new("settings.json"),
            ),
            [
                PathBuf::from("/stores/claude/settings.json"),
                PathBuf::from("/home/alice/.claude/settings.json"),
            ]
        );
        assert_eq!(
            claude_config_paths(
                Some(Path::new("/home/alice")),
                Some(Path::new("/home/alice/.claude")),
                Path::new(".claude/settings.json"),
                Path::new("settings.json"),
            ),
            [PathBuf::from("/home/alice/.claude/settings.json")]
        );
    }

    #[test]
    fn display_path_drops_the_verbatim_prefix() {
        assert_eq!(
            display_path(Path::new(
                r"\\?\C:\Users\me\AppData\Local\ai-memory\auth.json"
            )),
            r"C:\Users\me\AppData\Local\ai-memory\auth.json"
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\server\share\ai-memory")),
            r"\\server\share\ai-memory"
        );
        assert_eq!(
            display_path(Path::new("/home/alice/.local/share/ai-memory")),
            "/home/alice/.local/share/ai-memory"
        );
        assert_eq!(
            display_path(Path::new("/tmp/line\nbreak")),
            "/tmp/line?break"
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_handles_drive_paths() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\C:\Users\me\AppData\Local\ai-memory"),
            r"C:\Users\me\AppData\Local\ai-memory"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\c:\Users\me\AppData\Local\ai-memory"),
            r"c:\Users\me\AppData\Local\ai-memory"
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_handles_unc_case_insensitively() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\UNC\server\share\m"),
            r"\\server\share\m"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\unc\Server\Share\m"),
            r"\\Server\Share\m"
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_leaves_non_verbatim_paths_untouched() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"C:\Users\me\ai-memory"),
            r"C:\Users\me\ai-memory"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix("/home/alice/.local/share/ai-memory"),
            "/home/alice/.local/share/ai-memory"
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_leaves_unknown_forms_untouched() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\m"),
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\m"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\GLOBALROOT\Device\HarddiskVolume1\m"),
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\m"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\.\PhysicalDrive0"),
            r"\\.\PhysicalDrive0"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\C:relative\m"),
            r"\\?\C:relative\m"
        );
    }

    #[test]
    fn devin_user_config_dir_in_resolves_posix_and_windows_defaults() {
        let home = Path::new("/home/alice");
        assert_eq!(
            devin_user_config_dir_in(None, Some(home), false),
            Some(PathBuf::from("/home/alice/.config/devin"))
        );
        // POSIX ignores APPDATA even if set.
        assert_eq!(
            devin_user_config_dir_in(
                Some("C:\\Users\\Alice\\AppData\\Roaming".into()),
                Some(home),
                false
            ),
            Some(PathBuf::from("/home/alice/.config/devin"))
        );
        // Windows uses %APPDATA%\devin when set and non-empty.
        assert_eq!(
            devin_user_config_dir_in(
                Some("C:\\Users\\Alice\\AppData\\Roaming".into()),
                Some(home),
                true
            ),
            Some(PathBuf::from("C:\\Users\\Alice\\AppData\\Roaming").join("devin"))
        );
        // Windows falls back to <home>/.config/devin when APPDATA is unset or blank.
        assert_eq!(
            devin_user_config_dir_in(None, Some(home), true),
            Some(PathBuf::from("/home/alice/.config/devin"))
        );
        assert_eq!(
            devin_user_config_dir_in(Some("   ".into()), Some(home), true),
            Some(PathBuf::from("/home/alice/.config/devin"))
        );
        assert_eq!(devin_user_config_dir_in(None, None, false), None);
    }
}
