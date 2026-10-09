//! `install-hook` / `uninstall-hook`: a git `post-merge` hook that runs
//! `sync` after every merge or pull.
//!
//! The hook is a marked block, so it can share a file with other hooks and be
//! replaced or removed without touching them. It reports by default (a sync
//! dry-run) and applies only when installed with `--on-merge apply`; it never
//! passes `--prefer`, so a conflict always waits for a person. The companion
//! never runs git: the hooks directory is found by reading `.git` itself.
//!
//! The hook file is executable shell, so everything that reaches it is
//! single-quoted, and no token is ever written: at merge time `sync` reads
//! `AI_MEMORY_AUTH_TOKEN` from the environment like any other run.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{paths, sync};

pub const BEGIN_MARKER: &str = "# >>> ai-memory-wikisync >>>";
pub const END_MARKER: &str = "# <<< ai-memory-wikisync <<<";
const HOOK_NAME: &str = "post-merge";
const FRESH_SHEBANG: &str = "#!/bin/sh\n";
/// Interpreters that run the block's POSIX shell unchanged.
const SH_COMPATIBLE: &[&str] = &["sh", "bash", "dash", "ksh", "zsh", "ash"];

/// What the hook does after a merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnMerge {
    /// `sync` dry-run: print what would change.
    Report,
    /// `sync --apply`.
    Apply,
}

/// The `sync` invocation the hook runs.
#[derive(Debug, Clone)]
pub struct HookSpec {
    /// Written only when given explicitly; otherwise the hook uses
    /// `AI_MEMORY_SERVER_URL` or the default at merge time.
    pub server: Option<String>,
    pub workspace: String,
    pub project: String,
    pub dest: PathBuf,
    pub include: Vec<String>,
    pub on_merge: OnMerge,
    pub propagate_deletes: bool,
}

/// Arguments of `install-hook`.
#[derive(Debug, Clone)]
pub struct InstallArgs {
    pub spec: HookSpec,
    /// Print the block instead of writing it.
    pub print: bool,
    /// Install here instead of the repository's `.git/hooks`.
    pub hooks_dir: Option<PathBuf>,
    /// Add the block to an existing hook that does not have it yet.
    pub append: bool,
}

/// The git directories that matter for a hook.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GitDirs {
    /// The working tree root: git runs hooks from here.
    top: PathBuf,
    /// This worktree's git directory (`config.worktree` lives here).
    gitdir: PathBuf,
    /// The directory shared by all worktrees (`config`, `hooks`).
    common: PathBuf,
}

/// Single-quote one argument for `sh`. A newline or NUL could not survive a
/// one-line hook and has no business in these arguments, so it is refused.
fn quote(arg: &str) -> Result<String> {
    if arg.contains(['\n', '\r', '\0']) {
        bail!("refusing a hook argument with a newline or NUL: {arg:?}");
    }
    Ok(format!("'{}'", arg.replace('\'', r"'\''")))
}

/// The marked block, ending in a newline.
fn render_block(spec: &HookSpec, dest_arg: &str) -> Result<String> {
    let families = sync::validate_allowlist(&spec.include)?;
    let mut args: Vec<String> = vec![
        "--workspace".into(),
        quote(&spec.workspace)?,
        "--project".into(),
        quote(&spec.project)?,
        "--dest".into(),
        quote(dest_arg)?,
    ];
    for family in &families {
        args.push("--include".into());
        args.push(quote(family)?);
    }
    if let Some(server) = &spec.server {
        args.push("--server".into());
        args.push(quote(server)?);
    }
    if spec.on_merge == OnMerge::Apply {
        args.push("--apply".into());
    }
    if spec.propagate_deletes {
        args.push("--propagate-deletes".into());
    }
    Ok(format!(
        "{BEGIN_MARKER}\n\
         # Managed by `ai-memory-wikisync install-hook`; re-run it to change this block.\n\
         # A missing binary or a failed sync never fails the merge.\n\
         ( command -v ai-memory-wikisync >/dev/null && ai-memory-wikisync sync {} ) || true\n\
         {END_MARKER}\n",
        args.join(" ")
    ))
}

/// Walk up from `start` to the first `.git`: a directory, or a worktree's
/// `gitdir:` file whose git directory may name a shared `commondir`.
fn find_git(start: &Path) -> Result<Option<GitDirs>> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        let dot_git = current.join(".git");
        match fs::symlink_metadata(&dot_git) {
            Ok(meta) if meta.is_dir() => {
                return Ok(Some(GitDirs {
                    top: current.to_path_buf(),
                    gitdir: dot_git.clone(),
                    common: dot_git,
                }));
            }
            Ok(meta) if meta.is_file() => {
                let text = fs::read_to_string(&dot_git)
                    .with_context(|| format!("cannot read {}", dot_git.display()))?;
                let Some(target) = text.trim().strip_prefix("gitdir:") else {
                    bail!("{} is not a `gitdir:` file", dot_git.display());
                };
                let gitdir = paths::lexical_absolute(&current.join(target.trim()));
                let common = match fs::read_to_string(gitdir.join("commondir")) {
                    Ok(common) => paths::lexical_absolute(&gitdir.join(common.trim())),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => gitdir.clone(),
                    Err(e) => {
                        return Err(anyhow::Error::new(e))
                            .context(format!("cannot read {}/commondir", gitdir.display()));
                    }
                };
                return Ok(Some(GitDirs {
                    top: current.to_path_buf(),
                    gitdir,
                    common,
                }));
            }
            Ok(_) => bail!("{} is neither a directory nor a file", dot_git.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => dir = current.parent(),
            Err(e) => {
                return Err(anyhow::Error::new(e))
                    .context(format!("cannot inspect {}", dot_git.display()));
            }
        }
    }
    Ok(None)
}

/// The config file that sets `core.hooksPath`, if any. Git then ignores
/// `.git/hooks`, so a hook written there would silently never run.
fn hooks_path_config(git: &GitDirs) -> Result<Option<PathBuf>> {
    for config in [
        git.common.join("config"),
        git.gitdir.join("config.worktree"),
    ] {
        let text = match fs::read_to_string(&config) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(anyhow::Error::new(e))
                    .context(format!("cannot read {}", config.display()));
            }
        };
        if sets_hooks_path(&text) {
            return Ok(Some(config));
        }
    }
    Ok(None)
}

/// Whether a git config text sets `hooksPath` in its `[core]` section. Keys
/// and section names are case-insensitive in git.
fn sets_hooks_path(config: &str) -> bool {
    let mut in_core = false;
    for line in config.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[') {
            let name = section.split([']', ' ', '"']).next().unwrap_or_default();
            in_core = name.eq_ignore_ascii_case("core");
            continue;
        }
        let key = line.split(['=', ' ', '\t']).next().unwrap_or_default();
        if in_core && key.eq_ignore_ascii_case("hookspath") {
            return true;
        }
    }
    false
}

/// `--dest` as the hook passes it: relative to the working tree root, where
/// git runs hooks, so one hook serves every worktree of the repository.
fn dest_arg(dest: &Path, git: Option<&GitDirs>) -> Result<String> {
    let absolute = paths::lexical_absolute(dest);
    let shown = match git.and_then(|git| absolute.strip_prefix(&git.top).ok()) {
        Some(relative) if relative.as_os_str().is_empty() => PathBuf::from("."),
        Some(relative) => relative.to_path_buf(),
        None => absolute,
    };
    shown
        .to_str()
        .map(str::to_owned)
        .with_context(|| format!("--dest {} is not valid UTF-8", shown.display()))
}

fn hooks_dir(explicit: Option<&Path>, git: Option<&GitDirs>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return Ok(dir.to_path_buf());
    }
    let Some(git) = git else {
        bail!("no git repository found at or above --dest; pass --hooks-dir or --print");
    };
    if let Some(config) = hooks_path_config(git)? {
        bail!(
            "core.hooksPath is set in {}, so git ignores {}; pass --hooks-dir with that \
             directory, or --print and add the block to your hook manager",
            config.display(),
            git.common.join("hooks").display()
        );
    }
    Ok(git.common.join("hooks"))
}

/// The current hook text, or `None` when there is none. A symlinked hook is
/// refused: writing through it would change a file outside the hooks dir.
fn read_hook(hook: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(hook) {
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!("{} is a symlink; refusing to edit it", hook.display())
        }
        Ok(meta) if !meta.is_file() => bail!("{} is not a regular file", hook.display()),
        Ok(_) => fs::read_to_string(hook)
            .map(Some)
            .with_context(|| format!("cannot read {} as text", hook.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e)).context(format!("cannot inspect {}", hook.display())),
    }
}

/// Byte range of the marked block, through the end marker's line.
fn block_range(text: &str) -> Result<Option<(usize, usize)>> {
    let Some(begin) = text.find(BEGIN_MARKER) else {
        if text.contains(END_MARKER) {
            bail!("the hook has an end marker without a begin marker; fix it by hand");
        }
        return Ok(None);
    };
    let Some(end) = text[begin..].find(END_MARKER).map(|end| begin + end) else {
        bail!("the hook's ai-memory-wikisync block has no end marker; fix it by hand");
    };
    let end = text[end..]
        .find('\n')
        .map_or(text.len(), |newline| end + newline + 1);
    if text[end..].contains(BEGIN_MARKER) {
        bail!("the hook has more than one ai-memory-wikisync block; fix it by hand");
    }
    Ok(Some((begin, end)))
}

/// Whether a hook's shebang runs the block's POSIX shell.
fn sh_compatible(text: &str) -> bool {
    let Some(shebang) = text.lines().next().and_then(|line| line.strip_prefix("#!")) else {
        return false;
    };
    let mut words = shebang.split_whitespace();
    let program = words.next().unwrap_or_default();
    let name = program.rsplit('/').next().unwrap_or_default();
    let interpreter = if name == "env" {
        words.next().unwrap_or_default()
    } else {
        name
    };
    SH_COMPATIBLE.contains(&interpreter)
}

/// The hook text with `block` installed: a fresh file, a replaced block, or
/// (with `append`) the block after an existing shell hook.
fn merge_hook(existing: Option<&str>, block: &str, append: bool) -> Result<String> {
    let Some(text) = existing else {
        return Ok(format!("{FRESH_SHEBANG}{block}"));
    };
    if let Some((begin, end)) = block_range(text)? {
        return Ok(format!("{}{block}{}", &text[..begin], &text[end..]));
    }
    if !append {
        bail!(
            "a post-merge hook already exists without an ai-memory-wikisync block; pass \
             --append to add the block to it, or --print to add it yourself"
        );
    }
    if !sh_compatible(text) {
        bail!(
            "the existing post-merge hook does not start with a POSIX shell shebang \
             (sh, bash, dash, ksh, zsh); use --print and add the block yourself"
        );
    }
    let separator = if text.ends_with('\n') { "" } else { "\n" };
    Ok(format!("{text}{separator}{block}"))
}

/// Replace the hook atomically (tmp + fsync + rename) with mode 0755.
fn write_hook(hook: &Path, text: &str) -> Result<()> {
    let dir = hook.parent().context("hook path has no directory")?;
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join(".post-merge.wikisync-tmp");
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::Error::new(e))
                .context(format!("cannot clear stale {}", tmp.display()));
        }
    }
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("cannot create {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
                .with_context(|| format!("cannot make {} executable", tmp.display()))?;
        }
        file.write_all(text.as_bytes())
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("cannot sync {}", tmp.display()))?;
    }
    fs::rename(&tmp, hook).with_context(|| format!("cannot move {} into place", tmp.display()))
}

/// Entry point behind `install-hook`.
pub fn install(args: &InstallArgs) -> Result<()> {
    let git = find_git(&paths::lexical_absolute(&args.spec.dest))?;
    let block = render_block(&args.spec, &dest_arg(&args.spec.dest, git.as_ref())?)?;
    if args.print {
        print!("{block}");
        return Ok(());
    }
    let hook = hooks_dir(args.hooks_dir.as_deref(), git.as_ref())?.join(HOOK_NAME);
    let existing = read_hook(&hook)?;
    write_hook(
        &hook,
        &merge_hook(existing.as_deref(), &block, args.append)?,
    )?;
    let action = match args.spec.on_merge {
        OnMerge::Report => "report what a sync would change",
        OnMerge::Apply => "apply a sync",
    };
    println!(
        "installed {}: after each merge it will {action}",
        hook.display()
    );
    Ok(())
}

/// Entry point behind `uninstall-hook`: removes only the marked block, and
/// the file too when nothing but a shebang would remain.
pub fn uninstall(dest: &Path, explicit_hooks_dir: Option<&Path>) -> Result<()> {
    let git = find_git(&paths::lexical_absolute(dest))?;
    let hook = hooks_dir(explicit_hooks_dir, git.as_ref())?.join(HOOK_NAME);
    let Some(text) = read_hook(&hook)? else {
        println!("no {} hook; nothing to remove", hook.display());
        return Ok(());
    };
    let Some((begin, end)) = block_range(&text)? else {
        println!(
            "{} has no ai-memory-wikisync block; nothing to remove",
            hook.display()
        );
        return Ok(());
    };
    let rest = format!("{}{}", &text[..begin], &text[end..]);
    let mut lines = rest.lines().filter(|line| !line.trim().is_empty());
    let only_shebang = match lines.next() {
        None => true,
        Some(first) => first.starts_with("#!") && lines.next().is_none(),
    };
    if only_shebang {
        fs::remove_file(&hook).with_context(|| format!("cannot remove {}", hook.display()))?;
        println!("removed {}", hook.display());
    } else {
        write_hook(&hook, &rest)?;
        println!(
            "removed the ai-memory-wikisync block from {}",
            hook.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(dest: &Path) -> HookSpec {
        HookSpec {
            server: None,
            workspace: "demo".into(),
            project: "app".into(),
            dest: dest.to_path_buf(),
            include: vec!["decisions".into()],
            on_merge: OnMerge::Report,
            propagate_deletes: false,
        }
    }

    fn install_args(dest: &Path) -> InstallArgs {
        InstallArgs {
            spec: spec(dest),
            print: false,
            hooks_dir: None,
            append: false,
        }
    }

    /// A repository root with a bare-bones `.git` directory: all the hook
    /// installer reads. No git binary is involved.
    fn repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("repo");
        fs::create_dir_all(root.join(".git/hooks")).unwrap();
        fs::write(root.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
        fs::create_dir_all(root.join("docs/wiki")).unwrap();
        (tmp, root)
    }

    fn hook_of(root: &Path) -> PathBuf {
        root.join(".git/hooks/post-merge")
    }

    #[test]
    fn quoting_is_inert_and_refuses_newlines() {
        assert_eq!(quote("plain").unwrap(), "'plain'");
        assert_eq!(quote("it's").unwrap(), r"'it'\''s'");
        assert_eq!(quote("$(touch x)`y`").unwrap(), "'$(touch x)`y`'");
        assert!(quote("a\nb").is_err());
        assert!(quote("a\0b").is_err());
        assert!(quote("a\rb").is_err());
    }

    #[test]
    fn fresh_install_writes_an_executable_report_hook() {
        let (_tmp, root) = repo();
        install(&install_args(&root.join("docs/wiki"))).unwrap();
        let text = fs::read_to_string(hook_of(&root)).unwrap();
        assert!(text.starts_with("#!/bin/sh\n"), "{text}");
        assert!(
            text.contains(BEGIN_MARKER) && text.contains(END_MARKER),
            "{text}"
        );
        assert!(
            text.contains(
                "( command -v ai-memory-wikisync >/dev/null && ai-memory-wikisync sync \
                 --workspace 'demo' --project 'app' --dest 'docs/wiki' --include 'decisions' ) \
                 || true"
            ),
            "{text}"
        );
        for flag in [
            "--apply",
            "--prefer",
            "--propagate-deletes",
            "--server",
            "--token",
        ] {
            assert!(!text.contains(flag), "{flag} in a default hook: {text}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(hook_of(&root)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn opt_ins_reach_the_hook_and_prefer_never_does() {
        let (_tmp, root) = repo();
        let mut args = install_args(&root.join("docs/wiki"));
        args.spec.on_merge = OnMerge::Apply;
        args.spec.propagate_deletes = true;
        args.spec.server = Some("http://127.0.0.1:49374".into());
        install(&args).unwrap();
        let text = fs::read_to_string(hook_of(&root)).unwrap();
        assert!(
            text.contains(" --apply --propagate-deletes ) || true"),
            "{text}"
        );
        assert!(text.contains("--server 'http://127.0.0.1:49374'"), "{text}");
        assert!(!text.contains("--prefer"), "{text}");
    }

    #[test]
    fn reinstall_replaces_the_block_in_place() {
        let (_tmp, root) = repo();
        let args = install_args(&root.join("docs/wiki"));
        install(&args).unwrap();
        let first = fs::read_to_string(hook_of(&root)).unwrap();
        install(&args).unwrap();
        assert_eq!(fs::read_to_string(hook_of(&root)).unwrap(), first);

        let mut apply = args.clone();
        apply.spec.on_merge = OnMerge::Apply;
        install(&apply).unwrap();
        let text = fs::read_to_string(hook_of(&root)).unwrap();
        assert_eq!(text.matches(BEGIN_MARKER).count(), 1, "{text}");
        assert!(text.contains("--apply"), "{text}");
    }

    #[test]
    fn a_foreign_hook_is_kept_unless_appended_to_with_a_shell_shebang() {
        let (_tmp, root) = repo();
        let foreign = "#!/usr/bin/env bash\necho other hook\n";
        fs::write(hook_of(&root), foreign).unwrap();
        let mut args = install_args(&root.join("docs/wiki"));
        let err = install(&args).unwrap_err().to_string();
        assert!(err.contains("--append"), "{err}");
        assert_eq!(fs::read_to_string(hook_of(&root)).unwrap(), foreign);

        args.append = true;
        install(&args).unwrap();
        let text = fs::read_to_string(hook_of(&root)).unwrap();
        assert!(text.starts_with(foreign), "{text}");
        assert!(text.contains(BEGIN_MARKER), "{text}");
        // Appending again replaces the block instead of adding a second.
        install(&args).unwrap();
        assert_eq!(fs::read_to_string(hook_of(&root)).unwrap(), text);

        uninstall(&root.join("docs/wiki"), None).unwrap();
        assert_eq!(fs::read_to_string(hook_of(&root)).unwrap(), foreign);
    }

    #[test]
    fn append_refuses_a_hook_that_is_not_shell() {
        let (_tmp, root) = repo();
        for foreign in [
            "#!/usr/bin/env python3\nprint('x')\n",
            "#!/usr/bin/node\n",
            "echo no shebang\n",
        ] {
            fs::write(hook_of(&root), foreign).unwrap();
            let mut args = install_args(&root.join("docs/wiki"));
            args.append = true;
            let err = install(&args).unwrap_err().to_string();
            assert!(err.contains("shebang"), "{foreign:?}: {err}");
            assert_eq!(fs::read_to_string(hook_of(&root)).unwrap(), foreign);
        }
        assert!(sh_compatible("#!/bin/sh -e\n"));
        assert!(sh_compatible("#!/usr/bin/env zsh\n"));
    }

    #[test]
    fn uninstall_removes_a_hook_it_created() {
        let (_tmp, root) = repo();
        install(&install_args(&root.join("docs/wiki"))).unwrap();
        uninstall(&root.join("docs/wiki"), None).unwrap();
        assert!(!hook_of(&root).exists());
        // Nothing to remove is not an error.
        uninstall(&root.join("docs/wiki"), None).unwrap();
    }

    #[test]
    fn hooks_path_is_refused_unless_the_dir_is_given() {
        let (_tmp, root) = repo();
        fs::write(
            root.join(".git/config"),
            "[core]\n\tbare = false\n[Core]\n\tHooksPath = .githooks\n",
        )
        .unwrap();
        let mut args = install_args(&root.join("docs/wiki"));
        let err = install(&args).unwrap_err().to_string();
        assert!(err.contains("core.hooksPath"), "{err}");
        assert!(!hook_of(&root).exists());

        args.hooks_dir = Some(root.join(".githooks"));
        install(&args).unwrap();
        assert!(root.join(".githooks/post-merge").exists());

        assert!(!sets_hooks_path(
            "[core]\n\tbare = false\n[hooks]\n\thookspath = x\n"
        ));
        assert!(sets_hooks_path("[core]\nhooksPath=x\n"));
    }

    #[test]
    fn a_worktree_installs_into_the_shared_hooks_dir() {
        let (_tmp, root) = repo();
        let wt_gitdir = root.join(".git/worktrees/feature");
        fs::create_dir_all(&wt_gitdir).unwrap();
        fs::write(wt_gitdir.join("commondir"), "../..\n").unwrap();
        let worktree = root.parent().unwrap().join("feature");
        fs::create_dir_all(worktree.join("docs/wiki")).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .unwrap();

        install(&install_args(&worktree.join("docs/wiki"))).unwrap();
        let text = fs::read_to_string(hook_of(&root)).unwrap();
        assert!(text.contains("--dest 'docs/wiki'"), "{text}");

        // A worktree config can set hooksPath too.
        fs::write(wt_gitdir.join("config.worktree"), "[core]\nhooksPath = x\n").unwrap();
        let err = install(&install_args(&worktree.join("docs/wiki")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("core.hooksPath"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_hook_is_refused() {
        let (tmp, root) = repo();
        let elsewhere = tmp.path().join("elsewhere.sh");
        fs::write(&elsewhere, "#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink(&elsewhere, hook_of(&root)).unwrap();
        let mut args = install_args(&root.join("docs/wiki"));
        args.append = true;
        let err = install(&args).unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");
        assert_eq!(fs::read_to_string(&elsewhere).unwrap(), "#!/bin/sh\n");
    }

    #[test]
    fn print_writes_nothing() {
        let (_tmp, root) = repo();
        let mut args = install_args(&root.join("docs/wiki"));
        args.print = true;
        install(&args).unwrap();
        assert!(!hook_of(&root).exists());
    }

    #[test]
    fn newline_arguments_are_refused_before_writing() {
        let (_tmp, root) = repo();
        let mut args = install_args(&root.join("docs/wiki"));
        args.spec.project = "app\nrm -rf ~".into();
        assert!(install(&args).is_err());
        assert!(!hook_of(&root).exists());
    }

    /// The installed hook, run by `sh` with a stand-in binary on PATH: a
    /// command substitution in a workspace name arrives as a literal
    /// argument and never runs.
    #[cfg(unix)]
    #[test]
    fn hostile_names_stay_inert_when_the_hook_runs() {
        use std::os::unix::fs::PermissionsExt;
        let (tmp, root) = repo();
        let pwned = tmp.path().join("pwned");
        let mut args = install_args(&root.join("docs/wiki"));
        args.spec.workspace = format!("$(touch {})'`touch {}`", pwned.display(), pwned.display());
        install(&args).unwrap();

        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("ai-memory-wikisync");
        let seen = tmp.path().join("argv");
        fs::write(
            &fake,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", seen.display()),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let status = std::process::Command::new("sh")
            .arg(hook_of(&root))
            .current_dir(&root)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .status()
            .unwrap();
        assert!(status.success());
        assert!(!pwned.exists(), "the substitution ran");
        let argv = fs::read_to_string(&seen).unwrap();
        let lines: Vec<&str> = argv.lines().collect();
        assert_eq!(lines[0], "sync");
        assert_eq!(lines[2], args.spec.workspace, "{argv}");
    }

    /// Without the binary on PATH the hook is a no-op that still succeeds.
    #[cfg(unix)]
    #[test]
    fn a_missing_binary_never_fails_the_merge() {
        let (tmp, root) = repo();
        install(&install_args(&root.join("docs/wiki"))).unwrap();
        let empty = tmp.path().join("empty-bin");
        fs::create_dir_all(&empty).unwrap();
        let status = std::process::Command::new("/bin/sh")
            .arg(hook_of(&root))
            .current_dir(&root)
            .env("PATH", &empty)
            .status()
            .unwrap();
        assert!(status.success());
    }
}
