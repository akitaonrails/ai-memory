//! `ai-memory install-skills` — install core-managed ai-memory Agent Skills.

use std::fs;
use std::path::{Path, PathBuf};

use ai_memory_core::routing_skills::{
    AGENTS_SKILL_DIR, CLAUDE_SKILL_DIR, DEVIN_SKILL_DIR, GROK_SKILL_DIR, MANAGED_MARKER,
    MANAGED_SKILLS, ManagedSkill, SKILLS_DIR,
};
use anyhow::{Context, Result, bail};

use crate::cli::{InstallSkillsAgent, InstallSkillsArgs, InstallSkillsScope};
use crate::commands::apply_shared::{ApplyOutcome, apply_atomic};
use crate::commands::install_mcp;
use crate::commands::path_util::{claude_config_dir, home_dir};
use crate::config::Config;

#[derive(Debug, Clone, Eq, PartialEq)]
struct TargetRoot {
    path: PathBuf,
}

impl TargetRoot {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct InstallReport {
    path: PathBuf,
    outcome: ApplyOutcome,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct PreparedInstall {
    plans: Vec<(PathBuf, &'static ManagedSkill)>,
}

/// Run the `install-skills` subcommand.
///
/// # Errors
/// Returns an error when target roots cannot be resolved, an unmanaged
/// same-name skill would be overwritten without `--force`, or a skill
/// file cannot be written.
pub fn run(_config: &Config, args: InstallSkillsArgs) -> Result<()> {
    let roots = resolve_target_roots_from_env(&args)?;

    if args.print {
        print_install_plan(&roots);
        return Ok(());
    }

    let prepared = prepare_install(&args)?;
    print_reports(apply_prepared_install(prepared)?);

    Ok(())
}

pub(super) fn prepare_install(args: &InstallSkillsArgs) -> Result<PreparedInstall> {
    let roots = resolve_target_roots_from_env(args)?;
    let plans = skill_file_plans(&roots);
    validate_overwrite_safety(&plans, args.force)?;
    Ok(PreparedInstall { plans })
}

pub(super) fn run_prepared(prepared: PreparedInstall) -> Result<()> {
    print_reports(apply_prepared_install(prepared)?);
    Ok(())
}

fn print_reports(reports: Vec<InstallReport>) {
    for report in reports {
        println!(
            "✓ {} {} ({})",
            report.outcome.verb(),
            report.path.display(),
            outcome_detail(report.outcome),
        );
    }
}

fn resolve_target_roots_from_env(args: &InstallSkillsArgs) -> Result<Vec<TargetRoot>> {
    let cwd = std::env::current_dir().context("getting CWD for install-skills target")?;
    let home = home_dir();
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    let grok_home = (args.scope == InstallSkillsScope::Global
        && args.agent == InstallSkillsAgent::Grok)
        .then(install_mcp::grok_home)
        .transpose()?;
    let claude_config_dir = claude_config_dir(std::env::var_os("CLAUDE_CONFIG_DIR"));
    resolve_target_roots_for_platform(
        args,
        &cwd,
        home.as_deref(),
        appdata.as_deref(),
        grok_home.as_deref(),
        claude_config_dir.as_deref(),
        SkillHostPlatform::current(),
    )
}

#[cfg(test)]
fn resolve_target_roots(
    args: &InstallSkillsArgs,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<Vec<TargetRoot>> {
    resolve_target_roots_for_platform(args, cwd, home, None, None, None, SkillHostPlatform::Other)
}

fn resolve_target_roots_for_platform(
    args: &InstallSkillsArgs,
    cwd: &Path,
    home: Option<&Path>,
    appdata: Option<&Path>,
    grok_home: Option<&Path>,
    claude_config_dir: Option<&Path>,
    platform: SkillHostPlatform,
) -> Result<Vec<TargetRoot>> {
    if let Some(target_dir) = &args.target_dir {
        return Ok(vec![TargetRoot::new(target_dir.clone())]);
    }

    let roots = match args.agent {
        InstallSkillsAgent::ClaudeCode => {
            vec![agent_root(
                args.scope,
                SkillRootKind::Claude,
                cwd,
                home,
                appdata,
                grok_home,
                claude_config_dir,
                platform,
            )?]
        }
        InstallSkillsAgent::Agents => {
            vec![agent_root(
                args.scope,
                SkillRootKind::Agents,
                cwd,
                home,
                appdata,
                grok_home,
                claude_config_dir,
                platform,
            )?]
        }
        InstallSkillsAgent::Devin => {
            vec![agent_root(
                args.scope,
                SkillRootKind::Devin,
                cwd,
                home,
                appdata,
                grok_home,
                claude_config_dir,
                platform,
            )?]
        }
        InstallSkillsAgent::Grok => {
            vec![agent_root(
                args.scope,
                SkillRootKind::Grok,
                cwd,
                home,
                appdata,
                grok_home,
                claude_config_dir,
                platform,
            )?]
        }
        InstallSkillsAgent::Both => vec![
            agent_root(
                args.scope,
                SkillRootKind::Claude,
                cwd,
                home,
                appdata,
                grok_home,
                claude_config_dir,
                platform,
            )?,
            agent_root(
                args.scope,
                SkillRootKind::Agents,
                cwd,
                home,
                appdata,
                grok_home,
                claude_config_dir,
                platform,
            )?,
        ],
    };

    Ok(roots.into_iter().map(TargetRoot::new).collect())
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SkillHostPlatform {
    Windows,
    Other,
}

impl SkillHostPlatform {
    fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SkillRootKind {
    Claude,
    Agents,
    Devin,
    Grok,
}

#[allow(clippy::too_many_arguments)]
fn agent_root(
    scope: InstallSkillsScope,
    kind: SkillRootKind,
    cwd: &Path,
    home: Option<&Path>,
    appdata: Option<&Path>,
    grok_home: Option<&Path>,
    claude_config_dir: Option<&Path>,
    platform: SkillHostPlatform,
) -> Result<PathBuf> {
    if scope == InstallSkillsScope::Global
        && kind == SkillRootKind::Devin
        && platform == SkillHostPlatform::Windows
    {
        let appdata = appdata
            .context("could not locate %APPDATA% for global Devin skill install on Windows")?;
        return Ok(appdata.join("devin").join(SKILLS_DIR));
    }

    if scope == InstallSkillsScope::Global
        && kind == SkillRootKind::Grok
        && let Some(grok_home) = grok_home
    {
        return Ok(grok_home.join(SKILLS_DIR));
    }

    // Claude Code relocates its whole config dir under $CLAUDE_CONFIG_DIR;
    // global skills live at `$CLAUDE_CONFIG_DIR/skills`, not
    // `~/.claude/skills`. Project scope stays under the repo.
    if scope == InstallSkillsScope::Global
        && kind == SkillRootKind::Claude
        && let Some(claude_config_dir) = claude_config_dir
    {
        return Ok(claude_config_dir.join(SKILLS_DIR));
    }

    let base = match scope {
        InstallSkillsScope::Project => cwd,
        InstallSkillsScope::Global => {
            home.context("could not locate $HOME for global skill install")?
        }
    };
    let agent_dir = match kind {
        SkillRootKind::Claude => CLAUDE_SKILL_DIR,
        SkillRootKind::Agents => AGENTS_SKILL_DIR,
        SkillRootKind::Devin => DEVIN_SKILL_DIR,
        SkillRootKind::Grok => GROK_SKILL_DIR,
    };
    Ok(base.join(agent_dir).join(SKILLS_DIR))
}

#[cfg(test)]
fn install_managed_skills(roots: &[TargetRoot], force: bool) -> Result<Vec<InstallReport>> {
    let plans = skill_file_plans(roots);
    validate_overwrite_safety(&plans, force)?;
    apply_prepared_install(PreparedInstall { plans })
}

fn apply_prepared_install(prepared: PreparedInstall) -> Result<Vec<InstallReport>> {
    let mut reports = Vec::with_capacity(prepared.plans.len());
    for (path, skill) in prepared.plans {
        let outcome = apply_atomic(&path, |_| Ok(skill.content.to_string()))?;
        reports.push(InstallReport { path, outcome });
    }
    Ok(reports)
}

fn skill_file_plans(roots: &[TargetRoot]) -> Vec<(PathBuf, &'static ManagedSkill)> {
    let mut plans = Vec::with_capacity(roots.len() * MANAGED_SKILLS.len());
    for root in roots {
        for skill in MANAGED_SKILLS {
            plans.push((root.path.join(skill.relative_path), skill));
        }
    }
    plans
}

fn validate_overwrite_safety(
    plans: &[(PathBuf, &'static ManagedSkill)],
    force: bool,
) -> Result<()> {
    if force {
        return Ok(());
    }

    for (path, skill) in plans {
        if !path.exists() {
            continue;
        }
        let existing =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        if !existing.contains(MANAGED_MARKER) {
            bail!(
                "refusing to overwrite unmanaged skill `{}` at {}. \
                 Move or rename the existing skill, or rerun with --force to back it up and replace it.",
                skill.name,
                path.display()
            );
        }
    }

    Ok(())
}

fn print_install_plan(roots: &[TargetRoot]) {
    for root in roots {
        println!("# Skill root: {}\n", root.path.display());
        for skill in MANAGED_SKILLS {
            let path = root.path.join(skill.relative_path);
            println!("# Would write: {}\n", path.display());
            print!("{}", skill.content);
            if !skill.content.ends_with('\n') {
                println!();
            }
            println!();
        }
    }
}

fn outcome_detail(outcome: ApplyOutcome) -> &'static str {
    match outcome {
        ApplyOutcome::Created => "new file",
        ApplyOutcome::Updated => "backup written next to it",
        ApplyOutcome::NoOp => "already up to date",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use ai_memory_core::routing_skills::{MANAGED_MARKER, MANAGED_SKILLS};

    use super::*;
    use crate::cli::{InstallSkillsAgent, InstallSkillsArgs, InstallSkillsScope};
    use crate::commands::apply_shared::ApplyOutcome;

    fn args(scope: InstallSkillsScope, agent: InstallSkillsAgent) -> InstallSkillsArgs {
        InstallSkillsArgs {
            scope,
            agent,
            target_dir: None,
            print: false,
            force: false,
        }
    }

    fn root_names(roots: &[TargetRoot]) -> Vec<String> {
        roots
            .iter()
            .map(|root| root.path.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    #[test]
    fn resolves_project_and_global_skill_roots_for_each_agent_selection() {
        let cwd = Path::new("/repo");
        let home = Path::new("/home/alice");

        let project_claude = resolve_target_roots(
            &args(InstallSkillsScope::Project, InstallSkillsAgent::ClaudeCode),
            cwd,
            Some(home),
        )
        .unwrap();
        assert_eq!(root_names(&project_claude), ["/repo/.claude/skills"]);

        let project_agents = resolve_target_roots(
            &args(InstallSkillsScope::Project, InstallSkillsAgent::Agents),
            cwd,
            Some(home),
        )
        .unwrap();
        assert_eq!(root_names(&project_agents), ["/repo/.agents/skills"]);

        let project_devin = resolve_target_roots(
            &args(InstallSkillsScope::Project, InstallSkillsAgent::Devin),
            cwd,
            Some(home),
        )
        .unwrap();
        assert_eq!(root_names(&project_devin), ["/repo/.devin/skills"]);

        let project_grok = resolve_target_roots(
            &args(InstallSkillsScope::Project, InstallSkillsAgent::Grok),
            cwd,
            Some(home),
        )
        .unwrap();
        assert_eq!(root_names(&project_grok), ["/repo/.grok/skills"]);

        let project_both = resolve_target_roots(
            &args(InstallSkillsScope::Project, InstallSkillsAgent::Both),
            cwd,
            Some(home),
        )
        .unwrap();
        assert_eq!(
            root_names(&project_both),
            ["/repo/.claude/skills", "/repo/.agents/skills"]
        );

        let global_both = resolve_target_roots(
            &args(InstallSkillsScope::Global, InstallSkillsAgent::Both),
            cwd,
            Some(home),
        )
        .unwrap();
        assert_eq!(
            root_names(&global_both),
            ["/home/alice/.claude/skills", "/home/alice/.agents/skills"]
        );

        let global_devin = resolve_target_roots(
            &args(InstallSkillsScope::Global, InstallSkillsAgent::Devin),
            cwd,
            Some(home),
        )
        .unwrap();
        assert_eq!(root_names(&global_devin), ["/home/alice/.devin/skills"]);

        let global_grok = resolve_target_roots(
            &args(InstallSkillsScope::Global, InstallSkillsAgent::Grok),
            cwd,
            Some(home),
        )
        .unwrap();
        assert_eq!(root_names(&global_grok), ["/home/alice/.grok/skills"]);
    }

    #[test]
    fn devin_global_skill_root_matches_confirmed_windows_path() {
        let cwd = Path::new("/repo");
        let home = Path::new("/home/alice");
        let appdata = Path::new("C:/Users/Alice/AppData/Roaming");

        let global_devin = resolve_target_roots_for_platform(
            &args(InstallSkillsScope::Global, InstallSkillsAgent::Devin),
            cwd,
            Some(home),
            Some(appdata),
            None,
            None,
            SkillHostPlatform::Windows,
        )
        .unwrap();

        assert_eq!(
            root_names(&global_devin),
            ["C:/Users/Alice/AppData/Roaming/devin/skills"]
        );
    }

    #[test]
    fn devin_global_skill_root_requires_appdata_on_windows() {
        let cwd = Path::new("/repo");
        let home = Path::new("/home/alice");

        let err = resolve_target_roots_for_platform(
            &args(InstallSkillsScope::Global, InstallSkillsAgent::Devin),
            cwd,
            Some(home),
            None,
            None,
            None,
            SkillHostPlatform::Windows,
        )
        .unwrap_err();

        assert!(err.to_string().contains("%APPDATA%"));
    }

    #[test]
    fn global_grok_skill_root_uses_injected_grok_home_override() {
        let roots = resolve_target_roots_for_platform(
            &args(InstallSkillsScope::Global, InstallSkillsAgent::Grok),
            Path::new("/repo"),
            Some(Path::new("/home/alice")),
            None,
            Some(Path::new("/custom/grok")),
            None,
            SkillHostPlatform::Other,
        )
        .unwrap();
        assert_eq!(root_names(&roots), ["/custom/grok/skills"]);
    }

    #[test]
    fn global_claude_skill_root_uses_injected_claude_config_dir() {
        let roots = resolve_target_roots_for_platform(
            &args(InstallSkillsScope::Global, InstallSkillsAgent::ClaudeCode),
            Path::new("/repo"),
            Some(Path::new("/home/alice")),
            None,
            None,
            Some(Path::new("/stores/claude")),
            SkillHostPlatform::Other,
        )
        .unwrap();
        assert_eq!(root_names(&roots), ["/stores/claude/skills"]);
    }

    #[test]
    fn project_claude_skill_root_ignores_claude_config_dir() {
        let roots = resolve_target_roots_for_platform(
            &args(InstallSkillsScope::Project, InstallSkillsAgent::ClaudeCode),
            Path::new("/repo"),
            Some(Path::new("/home/alice")),
            None,
            None,
            Some(Path::new("/stores/claude")),
            SkillHostPlatform::Other,
        )
        .unwrap();
        assert_eq!(root_names(&roots), ["/repo/.claude/skills"]);
    }

    #[test]
    fn explicit_target_dir_overrides_scope_and_agent_roots() {
        let cwd = Path::new("/repo");
        let home = Path::new("/home/alice");
        let mut install_args = args(InstallSkillsScope::Global, InstallSkillsAgent::Both);
        install_args.target_dir = Some(PathBuf::from("/custom/skills"));

        let roots = resolve_target_roots(&install_args, cwd, Some(home)).unwrap();
        assert_eq!(root_names(&roots), ["/custom/skills"]);
    }

    #[test]
    fn install_is_idempotent_after_creating_managed_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = vec![TargetRoot::new(tmp.path().join(".claude/skills"))];

        let first = install_managed_skills(&roots, false).unwrap();
        assert_eq!(first.len(), MANAGED_SKILLS.len());
        assert!(
            first
                .iter()
                .all(|report| report.outcome == ApplyOutcome::Created)
        );

        let second = install_managed_skills(&roots, false).unwrap();
        assert_eq!(second.len(), MANAGED_SKILLS.len());
        assert!(
            second
                .iter()
                .all(|report| report.outcome == ApplyOutcome::NoOp)
        );
    }

    #[test]
    fn install_writes_managed_skills_to_devin_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".devin/skills");
        let reports = install_managed_skills(&[TargetRoot::new(root.clone())], false).unwrap();

        assert_eq!(reports.len(), MANAGED_SKILLS.len());
        for skill in MANAGED_SKILLS {
            let path = root.join(skill.relative_path);
            let content = fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("expected Devin skill {}: {err}", path.display()));
            assert!(content.contains(MANAGED_MARKER));
            assert!(content.contains(skill.description));
        }
    }

    #[test]
    fn refuses_to_overwrite_unmanaged_same_name_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".claude/skills");
        let unmanaged = root.join("ai-memory-retrieval/SKILL.md");
        fs::create_dir_all(unmanaged.parent().unwrap()).unwrap();
        fs::write(
            &unmanaged,
            "---\nname: ai-memory-retrieval\n---\nuser skill\n",
        )
        .unwrap();

        let err = install_managed_skills(&[TargetRoot::new(root.clone())], false).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("refusing to overwrite unmanaged skill"));
        assert!(message.contains("--force"));
        assert_eq!(
            fs::read_to_string(&unmanaged).unwrap(),
            "---\nname: ai-memory-retrieval\n---\nuser skill\n"
        );
        assert!(!root.join("ai-memory-handoff/SKILL.md").exists());
    }

    #[test]
    fn force_overwrites_unmanaged_same_name_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".claude/skills");
        let unmanaged = root.join("ai-memory-retrieval/SKILL.md");
        fs::create_dir_all(unmanaged.parent().unwrap()).unwrap();
        fs::write(
            &unmanaged,
            "---\nname: ai-memory-retrieval\n---\nuser skill\n",
        )
        .unwrap();

        let reports = install_managed_skills(&[TargetRoot::new(root)], true).unwrap();
        let overwritten = reports
            .iter()
            .find(|report| report.path == unmanaged)
            .expect("retrieval report");
        assert_eq!(overwritten.outcome, ApplyOutcome::Updated);
        assert!(
            fs::read_to_string(unmanaged)
                .unwrap()
                .contains(MANAGED_MARKER)
        );
    }
}
