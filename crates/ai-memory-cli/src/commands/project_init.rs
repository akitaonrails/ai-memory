//! `ai-memory project init` — create a checkout-local scope marker.
//!
//! This command deliberately stays local: it chooses a directory, suggests the
//! scope every client already resolves there, then creates `.ai-memory.toml`.
//! It does not create a server-side project or contact the configured server.

use std::io::{self, IsTerminal as _, Write as _};
use std::path::Path;

use ai_memory_core::MARKER_FILENAME;
use anyhow::{Context, Result, bail};

use crate::cli::ProjectInitArgs;
use crate::config::Config;

#[derive(Debug, PartialEq, Eq)]
struct Suggestions {
    workspace: String,
    project: String,
}

/// Run the interactive marker initializer.
///
/// # Errors
/// Returns an error outside an interactive terminal, when the current
/// directory or marker cannot be inspected, or when the marker cannot be
/// created without replacing an existing file.
pub fn run(config: &Config, _: ProjectInitArgs) -> Result<()> {
    let i18n = &config.i18n;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "{}",
            i18n.text("cli-project-init-ai-memory-project-init-needs-an-interactive-terminal")
        );
    }
    // The physical directory to write in (the Docker wrapper binds the
    // checkout at /work), and the scope every client already resolves for
    // this checkout: marker walk, then the git remote, then the basename. A
    // marker that names a different scope would split the project's memory.
    let start = match super::scope_directories(config) {
        Some((_, lookup_cwd)) => std::path::PathBuf::from(lookup_cwd),
        None => std::env::current_dir()
            .context(i18n.text("cli-profile-reading-the-current-directory"))?,
    };
    let (workspace, project) = super::resolve_scope(config, None, None)?;
    initialize(
        &config.i18n,
        &start,
        &Suggestions { workspace, project },
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
    )
}

fn initialize(
    i18n: &ai_memory_core::i18n::Localizer,
    start: &Path,
    suggestions: &Suggestions,
    input: &mut impl io::BufRead,
    output: &mut impl io::Write,
) -> Result<()> {
    let repo_root = ai_memory_consolidate::discover_repo_root(start).ok();
    let directory = repo_root.as_deref().unwrap_or(start);
    let marker = directory.join(MARKER_FILENAME);
    if marker
        .try_exists()
        .with_context(|| format!("checking {}", marker.display()))?
    {
        bail!(
            "{} already exists; refusing to overwrite it",
            marker.display()
        );
    }

    let workspace = prompt_scope_value(i18n, "Workspace", &suggestions.workspace, input, output)?;
    let project = prompt_scope_value(i18n, "Project", &suggestions.project, input, output)?;
    write_new_marker(&marker, &render_marker(&workspace, &project))?;
    writeln!(
        output,
        "{}",
        i18n.diagnostic(&format!("Created {}.", marker.display()))
    )
    .context("reporting created marker")?;
    if repo_root.is_some() {
        writeln!(
            output,
            "{}",
            i18n.diagnostic(
                "Commit it so every checkout and worktree of this repository routes the same way."
            )
        )
        .context("reporting created marker")?;
    }
    Ok(())
}

fn prompt_scope_value(
    i18n: &ai_memory_core::i18n::Localizer,
    label: &str,
    suggestion: &str,
    input: &mut impl io::BufRead,
    output: &mut impl io::Write,
) -> Result<String> {
    loop {
        write!(output, "{label} [{suggestion}]: ").context("prompting for marker scope")?;
        output.flush().context("flushing marker scope prompt")?;
        let mut line = String::new();
        if input
            .read_line(&mut line)
            .context("reading marker scope input")?
            == 0
        {
            bail!("aborted (no input)");
        }
        let value = if line.trim().is_empty() {
            suggestion
        } else {
            line.trim()
        };
        if valid_scope_value(value) {
            return Ok(value.to_owned());
        }
        writeln!(
            output,
            "{}",
            i18n.diagnostic(
                "Enter a non-empty name without '/', '\\', spaces or control characters."
            )
        )
        .context("reporting invalid marker scope")?;
    }
}

/// A scope name goes into URLs and paths everywhere, so refuse what would need
/// escaping there: separators, whitespace and control characters.
fn valid_scope_value(value: &str) -> bool {
    !value.is_empty()
        && !value
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_whitespace() || c.is_control())
}

/// Only the scope: every other marker key keeps the server's default (the
/// cross-project profile on, session-start briefing off) until the user opts
/// in by hand.
fn render_marker(workspace: &str, project: &str) -> String {
    let mut document = toml_edit::DocumentMut::new();
    document["workspace"] = toml_edit::value(workspace);
    document["project"] = toml_edit::value(project);
    document.to_string()
}

/// Write `body` to a new file at `path` atomically: a temporary sibling is
/// written and synced, then linked into place, which fails rather than
/// replace a marker that appeared meanwhile. A crash leaves no partial marker.
fn write_new_marker(path: &Path, body: &str) -> Result<()> {
    let dir = path
        .parent()
        .context("marker path has no parent directory")?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".ai-memory.toml.")
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    tmp.write_all(body.as_bytes())
        .with_context(|| format!("writing {}", tmp.path().display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("syncing {}", tmp.path().display()))?;
    match std::fs::hard_link(tmp.path(), path) {
        Ok(()) => {}
        // FAT/exFAT and some network or bind mounts refuse hard links. Fall
        // back to an exclusive create, which still never replaces a marker.
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .with_context(|| format!("creating {}", path.display()))?;
            file.write_all(body.as_bytes())
                .and_then(|()| file.sync_all())
                .with_context(|| format!("writing {} (after: {error})", path.display()))?;
        }
        Err(error) => {
            return Err(error).with_context(|| format!("creating {}", path.display()));
        }
    }
    if let Ok(dir) = std::fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::process::Command;

    use super::*;

    fn suggest(workspace: &str, project: &str) -> Suggestions {
        Suggestions {
            workspace: workspace.to_owned(),
            project: project.to_owned(),
        }
    }

    /// The suggestion is the scope the CLI and hooks already resolve. A
    /// checkout whose remote is `github.com/acme/api` is `default`/`acme-api`
    /// everywhere, so a marker suggesting `acme`/`api` would split its memory.
    #[test]
    fn a_remote_checkout_resolves_to_the_scope_every_client_uses() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("checkout");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &["remote", "add", "origin", "https://github.com/acme/api.git"],
        );
        let (workspace, project) =
            crate::commands::resolve_scope_for_path(&Config::default(), &repo).unwrap();
        assert_eq!(
            (workspace.as_str(), project.as_str()),
            ("default", "acme-api")
        );
    }

    /// An ancestor marker's workspace is inherited, so the suggestion keeps it.
    #[test]
    fn an_ancestor_workspace_marker_is_inherited() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(MARKER_FILENAME), "workspace = \"team\"\n").unwrap();
        let repo = temp.path().join("service");
        std::fs::create_dir(&repo).unwrap();
        let (workspace, _) =
            crate::commands::resolve_scope_for_path(&Config::default(), &repo).unwrap();
        assert_eq!(workspace, "team");
    }

    /// Only the scope is written; every other key keeps the server default.
    #[test]
    fn the_marker_holds_only_the_scope() {
        assert_eq!(
            render_marker("acme", "api"),
            "workspace = \"acme\"\nproject = \"api\"\n"
        );
    }

    #[test]
    fn initialize_writes_a_new_marker_from_entered_values() {
        let temp = tempfile::tempdir().unwrap();
        let mut input = Cursor::new(b"team\napi\n");
        let mut output = Vec::new();

        initialize(
            &ai_memory_core::i18n::Localizer::default(),
            temp.path(),
            &suggest("default", "x"),
            &mut input,
            &mut output,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(temp.path().join(MARKER_FILENAME)).unwrap(),
            render_marker("team", "api")
        );
        assert!(String::from_utf8(output).unwrap().contains("Created "));
        // No temporary sibling is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name() != MARKER_FILENAME)
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn initialize_accepts_the_suggested_values_on_empty_input() {
        let temp = tempfile::tempdir().unwrap();
        let mut input = Cursor::new(b"\n\n");
        let mut output = Vec::new();

        initialize(
            &ai_memory_core::i18n::Localizer::default(),
            temp.path(),
            &suggest("default", "acme-api"),
            &mut input,
            &mut output,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(temp.path().join(MARKER_FILENAME)).unwrap(),
            render_marker("default", "acme-api")
        );
    }

    #[test]
    fn initialize_from_a_worktree_descendant_writes_at_that_worktree_root() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "fixture@example.test"]);
        git(&main, &["config", "user.name", "Fixture"]);
        std::fs::write(main.join("README.md"), "fixture\n").unwrap();
        git(&main, &["add", "README.md"]);
        git(&main, &["commit", "--no-gpg-sign", "-qm", "initial"]);

        let worktree = temp.path().join("linked");
        git(
            &main,
            &["worktree", "add", "-q", worktree.to_str().unwrap(), "HEAD"],
        );
        let nested = worktree.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let mut input = Cursor::new(b"team\napi\n");
        let mut output = Vec::new();

        initialize(
            &ai_memory_core::i18n::Localizer::default(),
            &nested,
            &suggest("default", "x"),
            &mut input,
            &mut output,
        )
        .unwrap();

        assert!(worktree.join(MARKER_FILENAME).is_file());
        assert!(!main.join(MARKER_FILENAME).exists());
        assert!(String::from_utf8(output).unwrap().contains("Commit it"));
    }

    #[test]
    fn initialize_reprompts_for_names_that_need_escaping() {
        let temp = tempfile::tempdir().unwrap();
        let mut input = Cursor::new(b"team/api\nmy team\nteam\napi\\x\napi\n");
        let mut output = Vec::new();

        initialize(
            &ai_memory_core::i18n::Localizer::default(),
            temp.path(),
            &suggest("default", "x"),
            &mut input,
            &mut output,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(temp.path().join(MARKER_FILENAME)).unwrap(),
            render_marker("team", "api")
        );
        assert_eq!(
            String::from_utf8(output)
                .unwrap()
                .matches("Enter a non-empty name")
                .count(),
            3
        );
    }

    #[test]
    fn initialize_refuses_to_overwrite_an_existing_marker() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join(MARKER_FILENAME);
        std::fs::write(&marker, "workspace = \"original\"\n").unwrap();
        let mut input = Cursor::new(b"team\napi\n");
        let mut output = Vec::new();

        let error = initialize(
            &ai_memory_core::i18n::Localizer::default(),
            temp.path(),
            &suggest("default", "x"),
            &mut input,
            &mut output,
        )
        .unwrap_err();

        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            "workspace = \"original\"\n"
        );
        assert!(output.is_empty());
    }

    /// A marker that appears between the check and the write is not replaced.
    #[test]
    fn the_write_never_replaces_a_marker_that_appeared_meanwhile() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join(MARKER_FILENAME);
        std::fs::write(&marker, "workspace = \"raced\"\n").unwrap();
        assert!(write_new_marker(&marker, "workspace = \"new\"\n").is_err());
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "workspace = \"raced\"\n"
        );
    }

    #[test]
    fn initialize_aborts_on_end_of_input_without_creating_a_marker() {
        let temp = tempfile::tempdir().unwrap();
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        let error = initialize(
            &ai_memory_core::i18n::Localizer::default(),
            temp.path(),
            &suggest("default", "x"),
            &mut input,
            &mut output,
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "aborted (no input)");
        assert!(!temp.path().join(MARKER_FILENAME).exists());
    }

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("running git {args:?}: {error}"));
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
