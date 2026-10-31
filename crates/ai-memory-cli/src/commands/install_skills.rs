//! `ai-memory install-skills` — install core-managed ai-memory Agent Skills.

use std::fs;
use std::path::{Path, PathBuf};

use ai_memory_core::routing_skills::{
    AGENTS_SKILL_DIR, ANTIGRAVITY_APP_SKILL_DIR, ANTIGRAVITY_CLI_SKILL_DIR, ANTIGRAVITY_HOME_DIR,
    CLAUDE_SKILL_DIR, COPILOT_SKILL_DIR, DEVIN_SKILL_DIR, GITHUB_SKILL_DIR,
    GROK_HANDOFF_RULE_CONTENT, GROK_HANDOFF_RULE_FILENAME, GROK_HANDOFF_RULE_MARKER,
    GROK_RULES_DIR, GROK_SKILL_DIR, HERMES_SKILL_DIR, MANAGED_MARKER, MANAGED_SKILLS, ManagedSkill,
    SKILLS_DIR,
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
    /// `$GROK_HOME/rules/ai-memory-handoff.md` when installing global Grok
    /// skills. Project scope, `--target-dir`, and other agents leave this empty.
    grok_handoff_rule: Option<PathBuf>,
}

/// Run the `install-skills` subcommand.
///
/// # Errors
/// Returns an error when target roots cannot be resolved, an unmanaged
/// same-name skill would be overwritten without `--force`, or a skill
/// file cannot be written.
pub fn run(config: &Config, args: InstallSkillsArgs) -> Result<()> {
    let roots = resolve_target_roots_from_env(&args, &config.runtime_env)?;

    if args.print {
        print_install_plan(&config.i18n, &roots, grok_handoff_rule_path_for(&args)?);
        return Ok(());
    }

    let prepared = prepare_install(&args, &config.runtime_env)?;
    print_reports(apply_prepared_install(prepared)?);

    Ok(())
}

pub(super) fn prepare_install(
    args: &InstallSkillsArgs,
    env: &crate::config::RuntimeEnv,
) -> Result<PreparedInstall> {
    let roots = resolve_target_roots_from_env(args, env)?;
    let plans = skill_file_plans(&roots);
    let grok_handoff_rule = grok_handoff_rule_path_for(args)?;
    validate_overwrite_safety(&plans, args.force)?;
    if let Some(path) = &grok_handoff_rule {
        validate_rule_overwrite(path, args.force)?;
    }
    Ok(PreparedInstall {
        plans,
        grok_handoff_rule,
    })
}

/// After `install-hooks --agent grok --apply` writes the hook file, also
/// install the always-on first-turn rule, which the hook's first turn relies
/// on. An unmanaged same-name file is skipped with a warning so a
/// user-authored rule cannot fail the hook install. The global Grok skills are
/// `install-skills --scope global --agent grok`'s job, so `uninstall --only
/// hooks` removes everything this writes.
///
/// The companions go into the Grok home the hook file was written to, never
/// the process `GROK_HOME`: a relocated home (`ai-memory run grok --env
/// GROK_HOME=...`, or `--config-file`) must receive them, and a confined
/// install must not write outside it. A hook file outside the
/// `<grok home>/hooks/ai-memory.json` layout names no home, so the companions
/// are skipped with a note.
pub(super) fn install_grok_companions_after_hooks(
    i18n: &ai_memory_core::i18n::Localizer,
    hook_file: &Path,
) -> Result<()> {
    match grok_home_of_hook_file(hook_file) {
        Some(home) => {
            install_grok_rule_in(i18n, home, false);
            Ok(())
        }
        None => {
            eprintln!(
                "{}", i18n.format("cli-install-skills-ai-memory-note-skipped-the-grok-first-turn-rule-is-not-grok-home-hooks-ai-me", &[("value0", format!("{}", hook_file.display()))]));
            Ok(())
        }
    }
}

/// The Grok home a hook file at `<home>/hooks/ai-memory.json` belongs to.
fn grok_home_of_hook_file(hook_file: &Path) -> Option<&Path> {
    let hooks_dir = hook_file.parent()?;
    (hook_file.file_name()? == "ai-memory.json" && hooks_dir.file_name()? == "hooks")
        .then(|| hooks_dir.parent())
        .flatten()
}

fn install_grok_rule_in(i18n: &ai_memory_core::i18n::Localizer, grok_home: &Path, force: bool) {
    let rule_path = grok_handoff_rule_destination(grok_home);
    match write_grok_handoff_rule(&rule_path, force) {
        Ok(outcome) => {
            println!(
                "✓ {} {} ({})",
                outcome.verb(),
                rule_path.display(),
                outcome_detail(outcome),
            );
        }
        Err(err) => {
            eprintln!(
                "{}",
                i18n.format(
                    "cli-install-skills-ai-memory-warning-skipped-grok-first-turn-handoff-rule-err",
                    &[("value0", format!("{}", err))]
                )
            );
        }
    }
}

fn grok_handoff_rule_destination(grok_home: &Path) -> PathBuf {
    grok_home
        .join(GROK_RULES_DIR)
        .join(GROK_HANDOFF_RULE_FILENAME)
}

fn should_install_grok_handoff_rule(args: &InstallSkillsArgs) -> bool {
    args.target_dir.is_none()
        && args.scope == InstallSkillsScope::Global
        && args.agent == InstallSkillsAgent::Grok
}

fn grok_handoff_rule_path_for(args: &InstallSkillsArgs) -> Result<Option<PathBuf>> {
    if !should_install_grok_handoff_rule(args) {
        return Ok(None);
    }
    Ok(Some(grok_handoff_rule_destination(
        &install_mcp::grok_home()?,
    )))
}

fn write_grok_handoff_rule(path: &Path, force: bool) -> Result<ApplyOutcome> {
    validate_rule_overwrite(path, force)?;
    apply_atomic(path, |_| Ok(GROK_HANDOFF_RULE_CONTENT.to_string()))
}

pub(super) fn run_prepared(prepared: PreparedInstall) -> Result<()> {
    print_reports(apply_prepared_install(prepared)?);
    Ok(())
}

pub(super) fn run_prepared_quiet(prepared: PreparedInstall) -> Result<()> {
    let _ = apply_prepared_install(prepared)?;
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

/// The harness homes come from the environment `Config::load` captured, not
/// the live process (invariant #1).
fn resolve_target_roots_from_env(
    args: &InstallSkillsArgs,
    env: &crate::config::RuntimeEnv,
) -> Result<Vec<TargetRoot>> {
    let cwd = std::env::current_dir().context("getting CWD for install-skills target")?;
    let home = home_dir();
    let appdata = env.var_os("APPDATA").map(PathBuf::from);
    // Grok and Copilot CLI relocate their whole config home, global skills
    // included, through GROK_HOME / COPILOT_HOME.
    let agent_home = match (args.scope, args.agent) {
        (InstallSkillsScope::Global, InstallSkillsAgent::Grok) => {
            Some(install_mcp::grok_home_in(env.var_os("GROK_HOME"))?)
        }
        (InstallSkillsScope::Global, InstallSkillsAgent::CopilotCli) => {
            Some(install_mcp::copilot_home_in(env.var_os("COPILOT_HOME"))?)
        }
        _ => None,
    };
    let claude_config_dir = claude_config_dir(env.var_os("CLAUDE_CONFIG_DIR"));
    resolve_target_roots_for_platform(
        args,
        &cwd,
        home.as_deref(),
        appdata.as_deref(),
        agent_home.as_deref(),
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
    agent_home: Option<&Path>,
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
                agent_home,
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
                agent_home,
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
                agent_home,
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
                agent_home,
                claude_config_dir,
                platform,
            )?]
        }
        InstallSkillsAgent::CopilotCli => {
            vec![agent_root(
                args.scope,
                SkillRootKind::CopilotCli,
                cwd,
                home,
                appdata,
                agent_home,
                claude_config_dir,
                platform,
            )?]
        }
        InstallSkillsAgent::Hermes => {
            vec![agent_root(
                args.scope,
                SkillRootKind::Hermes,
                cwd,
                home,
                appdata,
                agent_home,
                claude_config_dir,
                platform,
            )?]
        }
        // Antigravity reads workspace skills from the cross-agent root.
        InstallSkillsAgent::Antigravity if args.scope == InstallSkillsScope::Project => {
            vec![agent_root(
                args.scope,
                SkillRootKind::Agents,
                cwd,
                home,
                appdata,
                agent_home,
                claude_config_dir,
                platform,
            )?]
        }
        InstallSkillsAgent::Antigravity => antigravity_global_skill_roots(
            home.context("could not locate $HOME for global skill install")?,
        )
        .to_vec(),
        InstallSkillsAgent::Both => vec![
            agent_root(
                args.scope,
                SkillRootKind::Claude,
                cwd,
                home,
                appdata,
                agent_home,
                claude_config_dir,
                platform,
            )?,
            agent_root(
                args.scope,
                SkillRootKind::Agents,
                cwd,
                home,
                appdata,
                agent_home,
                claude_config_dir,
                platform,
            )?,
        ],
    };

    Ok(roots.into_iter().map(TargetRoot::new).collect())
}

/// Antigravity's global skill roots under `home`: `~/.gemini/config/skills`,
/// which the App and IDE read, and `~/.gemini/antigravity-cli/skills`, which
/// the CLI reads. Antigravity documents `.agents/skills` only as a workspace
/// root (<https://antigravity.google/docs/skills/>).
pub(super) fn antigravity_global_skill_roots(home: &Path) -> [PathBuf; 2] {
    let antigravity = home.join(ANTIGRAVITY_HOME_DIR);
    [
        antigravity.join(ANTIGRAVITY_APP_SKILL_DIR).join(SKILLS_DIR),
        antigravity.join(ANTIGRAVITY_CLI_SKILL_DIR).join(SKILLS_DIR),
    ]
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
    CopilotCli,
    Hermes,
}

#[allow(clippy::too_many_arguments)]
fn agent_root(
    scope: InstallSkillsScope,
    kind: SkillRootKind,
    cwd: &Path,
    home: Option<&Path>,
    appdata: Option<&Path>,
    agent_home: Option<&Path>,
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
        && matches!(kind, SkillRootKind::Grok | SkillRootKind::CopilotCli)
        && let Some(agent_home) = agent_home
    {
        return Ok(agent_home.join(SKILLS_DIR));
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
        // Copilot CLI reads project skills from the repository's `.github`
        // and personal skills from `~/.copilot`.
        SkillRootKind::CopilotCli => match scope {
            InstallSkillsScope::Project => GITHUB_SKILL_DIR,
            InstallSkillsScope::Global => COPILOT_SKILL_DIR,
        },
        SkillRootKind::Hermes => HERMES_SKILL_DIR,
    };
    Ok(base.join(agent_dir).join(SKILLS_DIR))
}

#[cfg(test)]
fn install_managed_skills(roots: &[TargetRoot], force: bool) -> Result<Vec<InstallReport>> {
    let plans = skill_file_plans(roots);
    validate_overwrite_safety(&plans, force)?;
    apply_prepared_install(PreparedInstall {
        plans,
        grok_handoff_rule: None,
    })
}

fn apply_prepared_install(prepared: PreparedInstall) -> Result<Vec<InstallReport>> {
    let mut reports = Vec::with_capacity(
        prepared.plans.len() + usize::from(prepared.grok_handoff_rule.is_some()),
    );
    for (path, skill) in prepared.plans {
        let outcome = apply_atomic(&path, |_| Ok(skill.content.to_string()))?;
        reports.push(InstallReport { path, outcome });
    }
    if let Some(path) = prepared.grok_handoff_rule {
        let outcome = apply_atomic(&path, |_| Ok(GROK_HANDOFF_RULE_CONTENT.to_string()))?;
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

fn validate_rule_overwrite(path: &Path, force: bool) -> Result<()> {
    if force || !path.exists() {
        return Ok(());
    }
    let existing =
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if !existing.contains(GROK_HANDOFF_RULE_MARKER) {
        bail!(
            "refusing to overwrite unmanaged Grok rule at {}. \
             Move or rename the existing file, or rerun with --force to back it up and replace it.",
            path.display()
        );
    }
    Ok(())
}

fn print_install_plan(
    i18n: &ai_memory_core::i18n::Localizer,
    roots: &[TargetRoot],
    grok_handoff_rule: Option<PathBuf>,
) {
    print!("{}", render_install_plan(i18n, roots, grok_handoff_rule));
}

fn render_install_plan(
    i18n: &ai_memory_core::i18n::Localizer,
    roots: &[TargetRoot],
    grok_handoff_rule: Option<PathBuf>,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let push_file = |out: &mut String, content: &str| {
        out.push_str(content);
        if !content.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    };
    for root in roots {
        let _ = writeln!(
            out,
            "{}",
            i18n.diagnostic(&format!("# Skill root: {}\n", root.path.display()))
        );
        for skill in MANAGED_SKILLS {
            let path = root.path.join(skill.relative_path);
            let _ = writeln!(
                out,
                "{}",
                i18n.diagnostic(&format!("# Would write: {}\n", path.display()))
            );
            push_file(&mut out, skill.content);
        }
    }
    if let Some(path) = grok_handoff_rule {
        let _ = writeln!(
            out,
            "{}",
            i18n.diagnostic(&format!(
                "# Would write Grok first-turn handoff rule: {}\n",
                path.display()
            ))
        );
        push_file(&mut out, GROK_HANDOFF_RULE_CONTENT);
    }
    out
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

    use ai_memory_core::routing_skills::{
        GROK_HANDOFF_RULE_CONTENT, GROK_HANDOFF_RULE_MARKER, MANAGED_MARKER, MANAGED_SKILLS,
    };

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

        let project_hermes = resolve_target_roots(
            &args(InstallSkillsScope::Project, InstallSkillsAgent::Hermes),
            cwd,
            Some(home),
        )
        .unwrap();
        assert_eq!(root_names(&project_hermes), ["/repo/.hermes/skills"]);

        let global_hermes = resolve_target_roots(
            &args(InstallSkillsScope::Global, InstallSkillsAgent::Hermes),
            cwd,
            Some(home),
        )
        .unwrap();
        assert_eq!(root_names(&global_hermes), ["/home/alice/.hermes/skills"]);

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

    /// Antigravity's global skills go to the two roots its docs list, one per
    /// product, never `~/.agents/skills`; its workspace root is the shared
    /// `.agents/skills` (<https://antigravity.google/docs/skills/>).
    #[test]
    fn antigravity_skill_roots_are_the_documented_workspace_and_global_roots() {
        let cwd = Path::new("/repo");
        let home = Path::new("/home/alice");

        let project = resolve_target_roots(
            &args(InstallSkillsScope::Project, InstallSkillsAgent::Antigravity),
            cwd,
            Some(home),
        )
        .unwrap();
        assert_eq!(root_names(&project), ["/repo/.agents/skills"]);

        let global = resolve_target_roots(
            &args(InstallSkillsScope::Global, InstallSkillsAgent::Antigravity),
            cwd,
            Some(home),
        )
        .unwrap();
        assert_eq!(
            root_names(&global),
            [
                "/home/alice/.gemini/config/skills",
                "/home/alice/.gemini/antigravity-cli/skills"
            ]
        );

        let error = resolve_target_roots(
            &args(InstallSkillsScope::Global, InstallSkillsAgent::Antigravity),
            cwd,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("$HOME"), "{error}");
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

    /// COPILOT_HOME and GROK_HOME come from the captured environment, never
    /// the live process: an injected value decides the global root.
    #[test]
    fn global_harness_homes_come_from_the_captured_environment() {
        let env = crate::config::RuntimeEnv::with_process_env_for_tests(&[
            ("COPILOT_HOME", "/injected/copilot"),
            ("GROK_HOME", "/injected/grok"),
        ]);
        let copilot = resolve_target_roots_from_env(
            &args(InstallSkillsScope::Global, InstallSkillsAgent::CopilotCli),
            &env,
        )
        .unwrap();
        assert_eq!(
            copilot[0].path,
            Path::new("/injected/copilot").join(SKILLS_DIR)
        );
        let grok = resolve_target_roots_from_env(
            &args(InstallSkillsScope::Global, InstallSkillsAgent::Grok),
            &env,
        )
        .unwrap();
        assert_eq!(grok[0].path, Path::new("/injected/grok").join(SKILLS_DIR));
    }

    #[test]
    fn copilot_cli_skill_roots_split_project_github_and_global_copilot_home() {
        let cwd = Path::new("/repo");
        let home = Path::new("/home/alice");
        let project = resolve_target_roots(
            &args(InstallSkillsScope::Project, InstallSkillsAgent::CopilotCli),
            cwd,
            Some(home),
        )
        .unwrap();
        assert_eq!(root_names(&project), ["/repo/.github/skills"]);

        let global = resolve_target_roots(
            &args(InstallSkillsScope::Global, InstallSkillsAgent::CopilotCli),
            cwd,
            Some(home),
        )
        .unwrap();
        assert_eq!(root_names(&global), ["/home/alice/.copilot/skills"]);

        let relocated = resolve_target_roots_for_platform(
            &args(InstallSkillsScope::Global, InstallSkillsAgent::CopilotCli),
            cwd,
            Some(home),
            None,
            Some(Path::new("/custom/copilot")),
            None,
            SkillHostPlatform::Other,
        )
        .unwrap();
        assert_eq!(root_names(&relocated), ["/custom/copilot/skills"]);

        // COPILOT_HOME never moves project skills out of the repository.
        let project_relocated = resolve_target_roots_for_platform(
            &args(InstallSkillsScope::Project, InstallSkillsAgent::CopilotCli),
            cwd,
            Some(home),
            None,
            Some(Path::new("/custom/copilot")),
            None,
            SkillHostPlatform::Other,
        )
        .unwrap();
        assert_eq!(root_names(&project_relocated), ["/repo/.github/skills"]);
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

    #[test]
    fn only_global_grok_without_target_dir_installs_the_handoff_rule() {
        assert!(should_install_grok_handoff_rule(&args(
            InstallSkillsScope::Global,
            InstallSkillsAgent::Grok,
        )));
        assert!(!should_install_grok_handoff_rule(&args(
            InstallSkillsScope::Project,
            InstallSkillsAgent::Grok,
        )));
        assert!(!should_install_grok_handoff_rule(&args(
            InstallSkillsScope::Global,
            InstallSkillsAgent::ClaudeCode,
        )));
        assert!(!should_install_grok_handoff_rule(&args(
            InstallSkillsScope::Global,
            InstallSkillsAgent::Both,
        )));
        let mut target_dir = args(InstallSkillsScope::Global, InstallSkillsAgent::Grok);
        target_dir.target_dir = Some(PathBuf::from("/custom/skills"));
        assert!(!should_install_grok_handoff_rule(&target_dir));
    }

    #[test]
    fn global_grok_install_writes_the_handoff_rule_next_to_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let grok_home = tmp.path();
        let skills_root = grok_home.join("skills");
        let rule_path = grok_handoff_rule_destination(grok_home);
        let reports = apply_prepared_install(PreparedInstall {
            plans: skill_file_plans(&[TargetRoot::new(skills_root.clone())]),
            grok_handoff_rule: Some(rule_path.clone()),
        })
        .unwrap();

        assert_eq!(reports.len(), MANAGED_SKILLS.len() + 1);
        assert!(
            reports
                .iter()
                .all(|report| report.outcome == ApplyOutcome::Created)
        );
        assert!(
            fs::read_to_string(&rule_path)
                .unwrap()
                .contains(GROK_HANDOFF_RULE_MARKER)
        );
        assert_eq!(
            fs::read_to_string(&rule_path).unwrap(),
            GROK_HANDOFF_RULE_CONTENT
        );
        for skill in MANAGED_SKILLS {
            assert!(skills_root.join(skill.relative_path).exists());
        }
        assert!(!grok_home.join(".grok/skills").exists());
    }

    #[test]
    fn project_grok_skill_install_does_not_plan_the_handoff_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".grok/skills");
        let reports = install_managed_skills(&[TargetRoot::new(root.clone())], false).unwrap();
        assert_eq!(reports.len(), MANAGED_SKILLS.len());
        assert!(
            !tmp.path()
                .join("rules")
                .join("ai-memory-handoff.md")
                .exists()
        );
        assert!(!root.join("ai-memory-handoff.md").exists());
    }

    #[test]
    fn refuses_to_overwrite_unmanaged_grok_handoff_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let rule_path = grok_handoff_rule_destination(tmp.path());
        fs::create_dir_all(rule_path.parent().unwrap()).unwrap();
        fs::write(&rule_path, "# my grok rule\n").unwrap();

        let err = write_grok_handoff_rule(&rule_path, false).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("refusing to overwrite unmanaged Grok rule"));
        assert!(message.contains("--force"));
        assert_eq!(fs::read_to_string(&rule_path).unwrap(), "# my grok rule\n");
    }

    #[test]
    fn force_overwrites_unmanaged_grok_handoff_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let rule_path = grok_handoff_rule_destination(tmp.path());
        fs::create_dir_all(rule_path.parent().unwrap()).unwrap();
        fs::write(&rule_path, "# my grok rule\n").unwrap();

        let outcome = write_grok_handoff_rule(&rule_path, true).unwrap();
        assert_eq!(outcome, ApplyOutcome::Updated);
        assert!(
            fs::read_to_string(&rule_path)
                .unwrap()
                .contains(GROK_HANDOFF_RULE_MARKER)
        );
    }

    #[test]
    fn grok_handoff_rule_install_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let rule_path = grok_handoff_rule_destination(tmp.path());
        assert_eq!(
            write_grok_handoff_rule(&rule_path, false).unwrap(),
            ApplyOutcome::Created
        );
        assert_eq!(
            write_grok_handoff_rule(&rule_path, false).unwrap(),
            ApplyOutcome::NoOp
        );
    }

    /// The hook install writes the first-turn rule into the Grok home its
    /// hook file belongs to, and nothing else: no global skills, so
    /// `uninstall --only hooks` takes back all of it.
    #[test]
    fn grok_hook_install_writes_only_the_rule_beside_its_hook_file() {
        let tmp = tempfile::tempdir().unwrap();
        let grok_home = tmp.path().join("relocated-grok");
        let hook_file = grok_home.join("hooks").join("ai-memory.json");

        install_grok_companions_after_hooks(
            &ai_memory_core::i18n::Localizer::default(),
            &hook_file,
        )
        .unwrap();

        assert!(
            fs::read_to_string(grok_handoff_rule_destination(&grok_home))
                .unwrap()
                .contains(GROK_HANDOFF_RULE_MARKER)
        );
        assert!(!grok_home.join(SKILLS_DIR).exists(), "no global skills");
    }

    /// A hook file outside `<home>/hooks/ai-memory.json` names no Grok home,
    /// so nothing is written beside it or anywhere else.
    #[test]
    fn grok_hook_file_outside_the_layout_installs_no_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let hook_file = tmp.path().join("custom").join("grok-hooks.json");

        install_grok_companions_after_hooks(
            &ai_memory_core::i18n::Localizer::default(),
            &hook_file,
        )
        .unwrap();

        assert_eq!(grok_home_of_hook_file(&hook_file), None);
        assert!(!tmp.path().join("rules").exists());
        assert!(!tmp.path().join("custom").join("rules").exists());
    }

    #[test]
    fn print_plan_includes_the_grok_handoff_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = vec![TargetRoot::new(tmp.path().join("skills"))];
        let rule = grok_handoff_rule_destination(tmp.path());
        let plan = render_install_plan(
            &ai_memory_core::i18n::Localizer::default(),
            &roots,
            Some(rule.clone()),
        );
        assert!(plan.contains(&format!(
            "# Would write Grok first-turn handoff rule: {}",
            rule.display()
        )));
        assert!(plan.contains(GROK_HANDOFF_RULE_MARKER));
        // Without a rule path (project scope, --target-dir) the plan names none.
        assert!(
            !render_install_plan(&ai_memory_core::i18n::Localizer::default(), &roots, None)
                .contains("first-turn handoff rule:")
        );
    }
}
