//! `ai-memory profile` — inspect and curate the cross-project profile
//! (`docs/cross-project-profile.md`).
//!
//! Thin HTTP client: `status`, `list`, `review` and `apply` read
//! `/admin/profile/*` and `rebuild` posts to it; `show` and `forget` resolve
//! the profile's scope through `status` and then use the ordinary
//! `/admin/read-page` and `/admin/delete-page`, so a profile entry is read and
//! removed exactly like any other page. `apply` is the only command that
//! writes a local file: the managed block of the repository's rules file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::apply_shared::{PrivateBackup, apply_atomic_with_backup, checkout_backup_stem};
use crate::cli::{ProfileApplyArgs, ProfileArgs, ProfileCommand, ProfileScopeArgs};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json, post_json};

/// Dispatch a `profile` subcommand.
///
/// # Errors
/// Transport failures, a non-root token on a multi-user server, or an entry
/// that does not exist.
pub async fn run(config: &Config, args: ProfileArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    match args.command {
        ProfileCommand::Status(scope) => status(&ep, &scope).await,
        ProfileCommand::List(scope) => list(&ep, &scope).await,
        ProfileCommand::Show { path, scope } => show(&ep, &scope, &path).await,
        ProfileCommand::Forget { path, scope } => forget(&ep, &scope, &path).await,
        ProfileCommand::Review(scope) => review(&ep, &scope).await,
        ProfileCommand::Rebuild => rebuild(&ep).await,
        ProfileCommand::Apply(args) => apply(&ep, config, &args).await,
    }
}

#[derive(Debug, Deserialize)]
struct ScopeNames {
    workspace: String,
    project: String,
}

#[derive(Debug, Deserialize)]
struct StatusResponse {
    enabled: String,
    share: String,
    effective_share: Option<String>,
    distinguishes_operators: bool,
    min_projects: u32,
    inject_on_session_start: bool,
    digest_max_bytes: usize,
    baseline_max_bytes: usize,
    apply_max_lines: usize,
    llm: bool,
    scope: Option<ScopeNames>,
    entries: usize,
    digest_bytes: usize,
    contribute_opt_outs: Vec<ScopeNames>,
}

fn scope_query(scope: &ProfileScopeArgs) -> Vec<(&'static str, &str)> {
    let mut query = vec![("workspace", scope.workspace.as_str())];
    if let Some(user) = scope.user.as_deref() {
        query.push(("user", user));
    }
    query
}

async fn fetch_status(ep: &ServerEndpoint, scope: &ProfileScopeArgs) -> Result<StatusResponse> {
    get_json(ep, "/admin/profile/status", &scope_query(scope))
        .await
        .context("reading the profile status")
}

async fn status(ep: &ServerEndpoint, scope: &ProfileScopeArgs) -> Result<()> {
    let i18n = &ep.i18n;
    let s = fetch_status(ep, scope).await?;
    let deployment = if s.distinguishes_operators {
        "multi-user"
    } else {
        "single-user"
    };
    match s.effective_share.as_deref() {
        Some(share) => println!(
            "{}",
            i18n.format(
                "cli-profile-profile-on-share-deployment-server",
                &[
                    ("value0", share.to_string()),
                    ("value1", deployment.to_string())
                ]
            )
        ),
        None => println!(
            "{}",
            i18n.format(
                "cli-profile-profile-off-deployment-server",
                &[("value0", deployment.to_string())]
            )
        ),
    }
    println!(
        "{}",
        i18n.format(
            "cli-profile-enabled-share",
            &[
                ("value0", s.enabled.to_string()),
                ("value1", s.share.to_string())
            ]
        )
    );
    match &s.scope {
        Some(names) => println!(
            "{}",
            i18n.message(
                "cli-profile-scope-entr-digest-of-bytes",
                &[
                    ("value0", (names.workspace.to_string()).into()),
                    ("value1", (names.project.to_string()).into()),
                    ("value2", (format!("{}", s.entries)).into()),
                    ("value3", (s.entries).into()),
                    ("value4", (format!("{}", s.digest_bytes)).into()),
                    ("value5", (format!("{}", s.digest_max_bytes)).into())
                ]
            )
        ),
        None if s.effective_share.as_deref() == Some("user") && scope.user.is_none() => {
            println!(
                "{}",
                i18n.text(
                    "cli-profile-scope-one-private-profile-per-user-pass-user-name-to-inspect-one"
                )
            );
        }
        None if s.effective_share.is_some() => {
            println!("{}", i18n.text("cli-profile-scope-no-profile-entries-yet"));
        }
        None => {}
    }
    println!(
        "{}",
        i18n.format(
            "cli-profile-digest-at-session-start-baseline-for-new-projects-bytes",
            &[
                (
                    "value0",
                    (if s.inject_on_session_start {
                        "on"
                    } else {
                        "off"
                    })
                    .to_string()
                ),
                ("value1", format!("{}", s.baseline_max_bytes))
            ]
        )
    );
    println!(
        "{}",
        i18n.format(
            "cli-profile-min-projects-apply-max-lines-llm",
            &[
                ("value0", format!("{}", s.min_projects)),
                ("value1", format!("{}", s.apply_max_lines)),
                ("value2", format!("{}", s.llm))
            ]
        )
    );
    if s.contribute_opt_outs.is_empty() {
        println!("{}", i18n.text("cli-profile-every-project-contributes"));
    } else {
        println!(
            "{}",
            i18n.text("cli-profile-projects-that-opted-out-profile-contribute-false")
        );
        for p in &s.contribute_opt_outs {
            println!("    {}/{}", p.workspace, p.project);
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ListEntry {
    path: String,
    statement: String,
    applies_to: Vec<String>,
    enforced_by: bool,
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    scope: Option<ScopeNames>,
    entries: Vec<ListEntry>,
}

async fn list(ep: &ServerEndpoint, scope: &ProfileScopeArgs) -> Result<()> {
    let i18n = &ep.i18n;
    let resp: ListResponse = get_json(ep, "/admin/profile/list", &scope_query(scope))
        .await
        .context(i18n.text("cli-profile-listing-the-profile"))?;
    let Some(names) = resp.scope else {
        println!("{}", i18n.text("cli-profile-no-profile-entries-yet"));
        return Ok(());
    };
    println!("{}/{}:", names.workspace, names.project);
    for entry in &resp.entries {
        let applies = if entry.applies_to.is_empty() {
            String::new()
        } else {
            format!(" [{}]", entry.applies_to.join(", "))
        };
        let enforced = if entry.enforced_by {
            " (enforced elsewhere; not in the digest)"
        } else {
            ""
        };
        println!("  {}{applies}{enforced}", entry.path);
        println!("      {}", entry.statement);
    }
    Ok(())
}

/// A profile path as the user may type it: with or without `profile/`.
fn profile_path(raw: &str) -> String {
    let raw = raw.trim().trim_start_matches('/');
    if raw.starts_with(ai_memory_core::profile::PROFILE_PATH_PREFIX) {
        raw.to_owned()
    } else {
        format!("{}{raw}", ai_memory_core::profile::PROFILE_PATH_PREFIX)
    }
}

async fn profile_scope(ep: &ServerEndpoint, scope: &ProfileScopeArgs) -> Result<ScopeNames> {
    let s = fetch_status(ep, scope).await?;
    match s.scope {
        Some(names) => Ok(names),
        None if s.effective_share.is_none() => bail!("the profile is off on this server"),
        None if s.effective_share.as_deref() == Some("user") && scope.user.is_none() => {
            bail!("this server keeps one profile per user; pass --user <name>")
        }
        None => bail!("there are no profile entries yet"),
    }
}

#[derive(Debug, Deserialize)]
struct PageContent {
    path: String,
    body: String,
}

async fn show(ep: &ServerEndpoint, scope: &ProfileScopeArgs, path: &str) -> Result<()> {
    let names = profile_scope(ep, scope).await?;
    let path = profile_path(path);
    let page: PageContent = get_json(
        ep,
        "/admin/read-page",
        &[
            ("workspace", names.workspace.as_str()),
            ("project", names.project.as_str()),
            ("path", path.as_str()),
        ],
    )
    .await
    .with_context(|| format!("reading {path}"))?;
    println!("# {} ({}/{})\n", page.path, names.workspace, names.project);
    println!("{}", page.body);
    Ok(())
}

#[derive(Debug, Deserialize)]
struct DeleteResponse {
    path: String,
    deleted: bool,
}

async fn forget(ep: &ServerEndpoint, scope: &ProfileScopeArgs, path: &str) -> Result<()> {
    let i18n = &ep.i18n;
    let names = profile_scope(ep, scope).await?;
    let path = profile_path(path);
    let resp: DeleteResponse = post_json(
        ep,
        "/admin/delete-page",
        &serde_json::json!({
            "workspace": names.workspace,
            "project": names.project,
            "path": path,
        }),
    )
    .await
    .with_context(|| format!("forgetting {path}"))?;
    if resp.deleted {
        println!(
            "{}",
            i18n.format(
                "cli-profile-forgot-the-git-history-keeps-it",
                &[("value0", resp.path.to_string())]
            )
        );
    } else {
        println!(
            "{}",
            i18n.format(
                "cli-profile-was-not-in-the-profile-nothing-changed",
                &[("value0", resp.path.to_string())]
            )
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ReviewEntry {
    path: String,
    statement: String,
    projects: Option<u64>,
    confidence: Option<f64>,
    last_seen: Option<String>,
    managed: bool,
    #[serde(default)]
    evidence: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct WaitingGroup {
    statement: String,
    projects: usize,
    needs: usize,
    #[serde(default)]
    contributors_needed: usize,
}

#[derive(Debug, Deserialize)]
struct ReviewResponse {
    share: Option<String>,
    entries: Vec<ReviewEntry>,
    waiting: Vec<WaitingGroup>,
    pending_updates: Vec<String>,
    hand_edited: Vec<String>,
    forgotten: Vec<String>,
}

async fn review(ep: &ServerEndpoint, scope: &ProfileScopeArgs) -> Result<()> {
    let i18n = &ep.i18n;
    let r: ReviewResponse = get_json(ep, "/admin/profile/review", &scope_query(scope))
        .await
        .context(i18n.text("cli-profile-reviewing-the-profile"))?;
    let Some(share) = r.share else {
        println!(
            "{}",
            i18n.text("cli-profile-the-profile-is-off-on-this-server")
        );
        return Ok(());
    };
    println!(
        "{}",
        i18n.message(
            "cli-profile-profile-share-entr",
            &[
                ("value0", (share.to_string()).into()),
                ("value1", (format!("{}", r.entries.len())).into()),
                ("value2", (r.entries.len()).into())
            ]
        )
    );
    for entry in &r.entries {
        let mut facts = Vec::new();
        if let Some(n) = entry.projects {
            facts.push(format!("{n} project{}", if n == 1 { "" } else { "s" }));
        }
        if let Some(c) = entry.confidence {
            facts.push(format!("confidence {c:.2}"));
        }
        if let Some(seen) = &entry.last_seen {
            facts.push(format!("last seen {seen}"));
        }
        facts.push(if entry.managed {
            "harvested".into()
        } else {
            "yours (never rewritten)".into()
        });
        println!("  {}  [{}]", entry.path, facts.join(", "));
        println!("      {}", entry.statement);
        if let Some(items) = entry.evidence.as_array() {
            for item in items.iter().take(3) {
                let quote = item
                    .get("quote")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let project = item
                    .get("project")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                println!("        \"{quote}\" ({project})");
            }
        }
    }
    if !r.pending_updates.is_empty() {
        println!(
            "{}",
            i18n.format(
                "cli-profile-next-pass-will-update",
                &[("value0", r.pending_updates.join(", ").to_string())]
            )
        );
    }
    if !r.waiting.is_empty() {
        println!(
            "{}",
            i18n.text(
                "cli-profile-not-in-the-profile-yet-needs-more-projects-or-say-it-as-a-general-rule"
            )
        );
        for group in &r.waiting {
            let people = if group.contributors_needed > 0 {
                format!(
                    ", {} more operator{} needed for a team profile",
                    group.contributors_needed,
                    if group.contributors_needed == 1 {
                        ""
                    } else {
                        "s"
                    }
                )
            } else {
                String::new()
            };
            println!(
                "{}",
                i18n.message(
                    "cli-profile-project-more-needed-people",
                    &[
                        ("value0", (group.statement.to_string()).into()),
                        ("value1", (format!("{}", group.projects)).into()),
                        ("value2", (group.projects).into()),
                        ("value3", (format!("{}", group.needs)).into()),
                        ("value4", (people.to_string()).into())
                    ]
                )
            );
        }
    }
    if !r.hand_edited.is_empty() {
        println!(
            "{}",
            i18n.format(
                "cli-profile-edited-by-hand-left-alone",
                &[("value0", r.hand_edited.join(", ").to_string())]
            )
        );
    }
    if !r.forgotten.is_empty() {
        println!(
            "{}",
            i18n.format(
                "cli-profile-forgotten-stays-out-until-you-say-it-again",
                &[("value0", r.forgotten.join(", ").to_string())]
            )
        );
    }
    println!(
        "{}", i18n.text("cli-profile-fix-a-wrong-entry-ai-memory-profile-forget-path-reword-it-by-editing-the-pag"));
    Ok(())
}

#[derive(Debug, Deserialize)]
struct RebuildReport {
    share: Option<String>,
    projects_harvested: usize,
    candidates_added: usize,
    entries_written: usize,
    entries_unchanged: usize,
    skipped_manual: Vec<String>,
    llm_calls: usize,
    llm_fallbacks: usize,
    errors: Vec<String>,
}

async fn rebuild(ep: &ServerEndpoint) -> Result<()> {
    let i18n = &ep.i18n;
    let r: RebuildReport = post_json(ep, "/admin/profile/rebuild", &serde_json::json!({}))
        .await
        .context(i18n.text("cli-profile-rebuilding-the-profile"))?;
    println!(
        "{}",
        i18n.message(
            "cli-profile-rebuilt-the-profile-read-project-new-candidate-entr-written-unchanged",
            &[
                (
                    "value0",
                    (r.share.as_deref().unwrap_or("?").to_string()).into()
                ),
                ("value1", (format!("{}", r.projects_harvested)).into()),
                ("value2", (r.projects_harvested).into()),
                ("value3", (format!("{}", r.candidates_added)).into()),
                ("value4", (r.candidates_added).into()),
                ("value5", (format!("{}", r.entries_written)).into()),
                ("value6", (r.entries_written).into()),
                ("value7", (format!("{}", r.entries_unchanged)).into())
            ]
        )
    );
    if r.llm_calls > 0 || r.llm_fallbacks > 0 {
        println!(
            "{}",
            i18n.format(
                "cli-profile-llm-calls-fell-back-to-the-zero-llm-path",
                &[
                    ("value0", format!("{}", r.llm_calls)),
                    ("value1", format!("{}", r.llm_fallbacks))
                ]
            )
        );
    }
    if !r.skipped_manual.is_empty() {
        println!(
            "{}",
            i18n.format(
                "cli-profile-left-alone-edited-by-hand",
                &[("value0", r.skipped_manual.join(", ").to_string())]
            )
        );
    }
    for error in &r.errors {
        println!(
            "{}",
            i18n.format("cli-profile-error-error", &[("value0", error.to_string())])
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ApplyResponse {
    consume: bool,
    lines: Vec<String>,
    omitted: usize,
}

/// `profile apply`: write (or `--remove`) the managed profile block in the
/// current repository's rules file.
async fn apply(ep: &ServerEndpoint, config: &Config, args: &ProfileApplyArgs) -> Result<()> {
    let i18n = &config.i18n;
    let cwd =
        std::env::current_dir().context(i18n.text("cli-profile-reading-the-current-directory"))?;
    let root = repository_root(&cwd);
    let target = choose_target(&root, args.target.as_deref());
    let backup = PrivateBackup {
        dir: config.data_dir.join("backups").join("profile-apply"),
        stem: checkout_backup_stem(&root),
    };
    if args.remove {
        return remove_block(i18n, &target, &backup);
    }
    if let Some(marker) = crate::marker::find_settings_marker(&cwd.to_string_lossy())
        && crate::marker::parse_toml_flag(&marker, "consume")
            .is_some_and(|v| crate::marker::is_falsy(&v))
    {
        bail!(
            "{}",
            i18n.format(
                "cli-profile-sets-profile-consume-false-so-this-project-does-not-take-the-profile",
                &[("value0", format!("{}", marker.display()))]
            )
        );
    }
    let (workspace, project) = super::resolve_scope(config, None, None)?;
    let mut query = vec![
        ("workspace", workspace.as_str()),
        ("project", project.as_str()),
    ];
    if let Some(user) = args.user.as_deref() {
        query.push(("user", user));
    }
    let resp: ApplyResponse = get_json(ep, "/admin/profile/apply", &query)
        .await
        .context(i18n.text("cli-profile-reading-the-profile-entries-for-this-project"))?;
    if !resp.consume {
        bail!(
            "{}", i18n.format("cli-profile-workspace-project-sets-profile-consume-false-so-it-does-not-take-the-profile", &[("value0", workspace.to_string()), ("value1", project.to_string())]));
    }
    if resp.lines.is_empty() {
        println!(
            "{}",
            i18n.format(
                "cli-profile-no-profile-entries-apply-to-workspace-project-nothing-written",
                &[
                    ("value0", workspace.to_string()),
                    ("value1", project.to_string())
                ]
            )
        );
        return Ok(());
    }
    let block = ai_memory_core::profile::render_profile_block(&resp.lines, resp.omitted);
    if args.dry_run {
        println!(
            "{}",
            i18n.format(
                "cli-profile-would-write-to-block",
                &[
                    ("value0", format!("{}", target.display())),
                    ("value1", block.to_string())
                ]
            )
        );
        return Ok(());
    }
    let outcome = apply_atomic_with_backup(&target, Some(&backup), |existing| {
        Ok(ai_memory_core::profile::merge_profile_block(
            existing, &block,
        ))
    })?;
    println!(
        "{}",
        i18n.message(
            "cli-profile-entr",
            &[
                ("value0", (format!("{}", target.display())).into()),
                ("value1", (outcome.verb().to_string()).into()),
                ("value2", (format!("{}", resp.lines.len())).into()),
                ("value3", (resp.lines.len()).into()),
                (
                    "value4",
                    ((if resp.omitted > 0 {
                        format!(", {} more over apply_max_lines", resp.omitted)
                    } else {
                        String::new()
                    })
                    .to_string())
                    .into()
                )
            ]
        )
    );
    Ok(())
}

fn remove_block(
    i18n: &ai_memory_core::i18n::Localizer,
    target: &Path,
    backup: &PrivateBackup,
) -> Result<()> {
    let existing = match std::fs::read_to_string(target) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "{}",
                i18n.format(
                    "cli-profile-does-not-exist-nothing-to-remove",
                    &[("value0", format!("{}", target.display()))]
                )
            );
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", target.display()));
        }
    };
    let Some(remaining) = ai_memory_core::profile::remove_profile_block(&existing) else {
        println!(
            "{}",
            i18n.format(
                "cli-profile-has-no-profile-block-nothing-to-remove",
                &[("value0", format!("{}", target.display()))]
            )
        );
        return Ok(());
    };
    if remaining.is_empty() {
        // The file held nothing but the block `apply` created it for.
        std::fs::remove_file(target).with_context(|| format!("removing {}", target.display()))?;
        println!(
            "{}",
            i18n.format(
                "cli-profile-removed-it-held-only-the-profile-block",
                &[("value0", format!("{}", target.display()))]
            )
        );
        return Ok(());
    }
    let outcome = apply_atomic_with_backup(target, Some(backup), |_| Ok(remaining))?;
    println!(
        "{}",
        i18n.format(
            "cli-profile-profile-block-removed",
            &[
                ("value0", format!("{}", target.display())),
                ("value1", outcome.verb().to_string())
            ]
        )
    );
    Ok(())
}

/// The checkout `cwd` is in: the nearest ancestor holding `.git` (a directory,
/// or the file a linked worktree has), else `cwd` itself.
fn repository_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd)
        .to_path_buf()
}

/// The rules file `apply` writes: `--target` (relative to `root`), else an
/// existing `AGENTS.md`, else an existing `CLAUDE.md` unless it only imports
/// `AGENTS.md` (writing there would be a no-op for every other harness), else
/// a new `AGENTS.md`.
fn choose_target(root: &Path, explicit: Option<&Path>) -> PathBuf {
    if let Some(explicit) = explicit {
        return if explicit.is_absolute() {
            explicit.to_path_buf()
        } else {
            root.join(explicit)
        };
    }
    let agents = root.join("AGENTS.md");
    if agents.exists() {
        return agents;
    }
    let claude = root.join("CLAUDE.md");
    match std::fs::read_to_string(&claude) {
        Ok(text) if !only_imports_agents(&text) => claude,
        _ => agents,
    }
}

/// A `CLAUDE.md` whose every non-blank line is an `@AGENTS.md` import.
fn only_imports_agents(text: &str) -> bool {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    lines.clone().next().is_some() && lines.all(|line| line == "@AGENTS.md")
}

#[cfg(test)]
mod tests {
    use super::{choose_target, only_imports_agents, profile_path, repository_root};

    #[test]
    fn profile_paths_gain_the_prefix_once() {
        assert_eq!(profile_path("tools/pnpm.md"), "profile/tools/pnpm.md");
        assert_eq!(
            profile_path("profile/tools/pnpm.md"),
            "profile/tools/pnpm.md"
        );
        assert_eq!(profile_path(" /tools/pnpm.md "), "profile/tools/pnpm.md");
    }

    #[test]
    fn the_target_prefers_agents_then_a_real_claude_md() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(
            choose_target(root, None),
            root.join("AGENTS.md"),
            "new AGENTS.md"
        );

        std::fs::write(root.join("CLAUDE.md"), "@AGENTS.md\n").unwrap();
        assert_eq!(
            choose_target(root, None),
            root.join("AGENTS.md"),
            "an import-only CLAUDE.md points at AGENTS.md"
        );
        std::fs::write(root.join("CLAUDE.md"), "# Rules\nUse tabs.\n").unwrap();
        assert_eq!(choose_target(root, None), root.join("CLAUDE.md"));

        std::fs::write(root.join("AGENTS.md"), "# Agents\n").unwrap();
        assert_eq!(choose_target(root, None), root.join("AGENTS.md"));
        assert_eq!(
            choose_target(root, Some(std::path::Path::new("docs/RULES.md"))),
            root.join("docs/RULES.md")
        );
    }

    #[test]
    fn import_only_detection() {
        assert!(only_imports_agents("@AGENTS.md\n"));
        assert!(only_imports_agents("\n  @AGENTS.md  \n\n"));
        assert!(!only_imports_agents(""));
        assert!(!only_imports_agents("@AGENTS.md\nAlso: use tabs.\n"));
    }

    #[test]
    fn the_root_is_the_nearest_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let nested = repo.join("crates").join("x");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();
        assert_eq!(repository_root(&nested), repo);
        let loose = dir.path().join("loose");
        std::fs::create_dir(&loose).unwrap();
        assert_eq!(repository_root(&loose), loose);
    }
}
