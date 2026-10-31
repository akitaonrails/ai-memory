//! Canonical managed ai-memory Agent Skill assets.
//!
//! The installer-facing crates consume these definitions so every path that
//! writes ai-memory routing skills uses the same metadata and `SKILL.md` bytes.
//! The Grok first-turn handoff rule is a sibling always-on user rule, not an
//! Agent Skill: Grok loads `$GROK_HOME/rules/*.md` before the first turn.

/// Stable ownership marker embedded in every managed ai-memory skill file.
pub const MANAGED_MARKER: &str = "<!-- ai-memory-managed: routing-skill -->";

/// Stable ownership marker embedded in the managed Grok first-turn handoff rule.
pub const GROK_HANDOFF_RULE_MARKER: &str = "<!-- ai-memory-managed: grok-handoff-rule -->";

/// Grok user-rules directory below `$GROK_HOME` (default `~/.grok/rules`).
pub const GROK_RULES_DIR: &str = "rules";

/// Filename of the managed Grok first-turn handoff rule.
pub const GROK_HANDOFF_RULE_FILENAME: &str = "ai-memory-handoff.md";

/// Complete contents of the managed Grok first-turn handoff rule.
pub const GROK_HANDOFF_RULE_CONTENT: &str = include_str!("routing_skills/grok-handoff-rule.md");

/// Claude-compatible Agent Skill directory below a project or home root.
pub const CLAUDE_SKILL_DIR: &str = ".claude";
/// AGENTS-aware cross-client Agent Skill directory below a project or home root.
pub const AGENTS_SKILL_DIR: &str = ".agents";
/// Devin-compatible Agent Skill directory below a project or home root.
pub const DEVIN_SKILL_DIR: &str = ".devin";
/// Grok Build CLI Agent Skill directory below a project or home root.
pub const GROK_SKILL_DIR: &str = ".grok";
/// GitHub Copilot CLI project Agent Skill directory below a repository root.
pub const GITHUB_SKILL_DIR: &str = ".github";
/// GitHub Copilot CLI personal Agent Skill directory below the home root, used
/// when `COPILOT_HOME` does not relocate it.
pub const COPILOT_SKILL_DIR: &str = ".copilot";
/// Hermes Agent Skill directory below a project or home root.
pub const HERMES_SKILL_DIR: &str = ".hermes";
/// Google Antigravity's directory below the home root. Its global skills live
/// in two directories under it, one per product; workspace skills use
/// [`AGENTS_SKILL_DIR`] (<https://antigravity.google/docs/skills/>).
pub const ANTIGRAVITY_HOME_DIR: &str = ".gemini";
/// Below [`ANTIGRAVITY_HOME_DIR`]: the Antigravity App and IDE global skills
/// parent, `~/.gemini/config`.
pub const ANTIGRAVITY_APP_SKILL_DIR: &str = "config";
/// Below [`ANTIGRAVITY_HOME_DIR`]: the Antigravity CLI global skills parent,
/// `~/.gemini/antigravity-cli`.
pub const ANTIGRAVITY_CLI_SKILL_DIR: &str = "antigravity-cli";
/// Leaf directory that contains individual Agent Skill directories.
pub const SKILLS_DIR: &str = "skills";

const RETRIEVAL_DESCRIPTION: &str = "Use this skill for any request whose goal is read-only retrieval from ai-memory: project history, prior context, decisions, rules, gotchas, recent activity, full wiki pages, or status/briefing. Trigger by semantic intent rather than exact wording, including when ai-memory is not named.";
const HANDOFF_DESCRIPTION: &str = "Use this skill for any request whose goal is session continuity across agents or time: finding a pending handoff, resuming previous work, saving next-session context, wrapping up, or discarding a mistaken handoff. Trigger by semantic intent rather than exact wording.";
const MESSAGING_DESCRIPTION: &str = "Use this skill for cross-project agent-to-agent messaging: sending a request to another project's inbox, checking or popping this project's inbox, or retracting a message you sent. Trigger by semantic intent (delegate to another repo, check my inbox, tell the other agent) rather than exact wording.";
const DURABLE_PAGES_DESCRIPTION: &str = "Use this skill for any explicit wiki mutation in ai-memory: saving durable or time-bounded project knowledge, recording a rule or annotation, updating a note, or deleting a memory page. Trigger by semantic intent rather than exact wording; routine session capture is not a durable-page request.";
const LEARNING_MAINTENANCE_DESCRIPTION: &str = "Use this skill for any ai-memory knowledge-base maintenance request: consolidating observations, reviewing session lessons, proposing durable learnings, auditing or linting the wiki, finding contradictions, pruning stale memory, or running auto-improvement. Trigger by semantic intent rather than exact wording.";
const ROUTING_INSTALL_DESCRIPTION: &str = "Use this skill for any request to install, refresh, repair, inspect, or remove ai-memory's agent-facing routing: managed instruction snippets, Agent Skills, CLAUDE.md/AGENTS.md integration, or local/global skill roots. Trigger by semantic intent rather than exact wording.";

/// One ai-memory-managed Agent Skill file bundled by the core crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManagedSkill {
    /// Skill directory name and frontmatter `name` value.
    pub name: &'static str,
    /// Trigger-rich frontmatter `description` value.
    pub description: &'static str,
    /// File path relative to an agent skill root.
    pub relative_path: &'static str,
    /// Complete `SKILL.md` contents.
    pub content: &'static str,
}

/// Canonical managed ai-memory routing skills.
pub const MANAGED_SKILLS: &[ManagedSkill] = &[
    ManagedSkill {
        name: "ai-memory-retrieval",
        description: RETRIEVAL_DESCRIPTION,
        relative_path: "ai-memory-retrieval/SKILL.md",
        content: include_str!("routing_skills/ai-memory-retrieval/SKILL.md"),
    },
    ManagedSkill {
        name: "ai-memory-handoff",
        description: HANDOFF_DESCRIPTION,
        relative_path: "ai-memory-handoff/SKILL.md",
        content: include_str!("routing_skills/ai-memory-handoff/SKILL.md"),
    },
    ManagedSkill {
        name: "ai-memory-messaging",
        description: MESSAGING_DESCRIPTION,
        relative_path: "ai-memory-messaging/SKILL.md",
        content: include_str!("routing_skills/ai-memory-messaging/SKILL.md"),
    },
    ManagedSkill {
        name: "ai-memory-durable-pages",
        description: DURABLE_PAGES_DESCRIPTION,
        relative_path: "ai-memory-durable-pages/SKILL.md",
        content: include_str!("routing_skills/ai-memory-durable-pages/SKILL.md"),
    },
    ManagedSkill {
        name: "ai-memory-learning-maintenance",
        description: LEARNING_MAINTENANCE_DESCRIPTION,
        relative_path: "ai-memory-learning-maintenance/SKILL.md",
        content: include_str!("routing_skills/ai-memory-learning-maintenance/SKILL.md"),
    },
    ManagedSkill {
        name: "ai-memory-routing-install",
        description: ROUTING_INSTALL_DESCRIPTION,
        relative_path: "ai-memory-routing-install/SKILL.md",
        content: include_str!("routing_skills/ai-memory-routing-install/SKILL.md"),
    },
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::path::{Component, Path};

    use super::{
        GROK_HANDOFF_RULE_CONTENT, GROK_HANDOFF_RULE_FILENAME, GROK_HANDOFF_RULE_MARKER,
        GROK_RULES_DIR, MANAGED_MARKER, MANAGED_SKILLS, ManagedSkill,
    };

    const EXPECTED_SKILLS: &[&str] = &[
        "ai-memory-retrieval",
        "ai-memory-handoff",
        "ai-memory-messaging",
        "ai-memory-durable-pages",
        "ai-memory-learning-maintenance",
        "ai-memory-routing-install",
    ];

    const EXPECTED_TOOL_CLUSTERS: &[(&str, &str)] = &[
        ("memory_query", "ai-memory-retrieval"),
        ("memory_recent", "ai-memory-retrieval"),
        ("memory_read_page", "ai-memory-retrieval"),
        ("memory_read_session_observations", "ai-memory-retrieval"),
        ("memory_status", "ai-memory-retrieval"),
        ("memory_briefing", "ai-memory-retrieval"),
        ("memory_explore", "ai-memory-retrieval"),
        ("memory_handoff_accept", "ai-memory-handoff"),
        ("memory_handoff_begin", "ai-memory-handoff"),
        ("memory_handoff_cancel", "ai-memory-handoff"),
        ("memory_handoff_list", "ai-memory-handoff"),
        ("memory_message_send", "ai-memory-messaging"),
        ("memory_message_list", "ai-memory-messaging"),
        ("memory_message_pop", "ai-memory-messaging"),
        ("memory_message_cancel", "ai-memory-messaging"),
        ("memory_resolve_write_target", "ai-memory-durable-pages"),
        ("memory_write_page", "ai-memory-durable-pages"),
        ("memory_write_pages_batch", "ai-memory-durable-pages"),
        ("memory_patch_page", "ai-memory-durable-pages"),
        ("memory_delete_page", "ai-memory-durable-pages"),
        ("memory_restore_page", "ai-memory-durable-pages"),
        ("memory_consolidate", "ai-memory-learning-maintenance"),
        ("memory_auto_improve", "ai-memory-learning-maintenance"),
        ("memory_lint", "ai-memory-learning-maintenance"),
        ("memory_forget_sweep", "ai-memory-learning-maintenance"),
        ("memory_install_self_routing", "ai-memory-routing-install"),
    ];
    const PROJECT_SCOPED_SKILLS: &[&str] = &[
        "ai-memory-retrieval",
        "ai-memory-handoff",
        "ai-memory-messaging",
        "ai-memory-durable-pages",
        "ai-memory-learning-maintenance",
    ];

    #[derive(Debug, serde::Deserialize)]
    struct Frontmatter {
        name: String,
        description: String,
    }

    #[test]
    fn exposes_exact_managed_skill_set() {
        let names: Vec<_> = MANAGED_SKILLS.iter().map(|skill| skill.name).collect();
        assert_eq!(names, EXPECTED_SKILLS);
    }

    #[test]
    fn skill_frontmatter_is_valid_and_matches_metadata() {
        for skill in MANAGED_SKILLS {
            let frontmatter = parse_frontmatter(skill);
            assert_eq!(frontmatter.name, skill.name);
            assert_eq!(frontmatter.description, skill.description);
            assert_eq!(frontmatter.name, directory_name(skill));
            assert!(!frontmatter.description.trim().is_empty());
            assert!(
                frontmatter.description.chars().count() <= 1024,
                "{} description is over the Agent Skills limit",
                skill.name
            );
        }
    }

    #[test]
    fn every_skill_has_managed_marker() {
        assert!(!MANAGED_MARKER.is_empty());
        for skill in MANAGED_SKILLS {
            assert!(
                skill.content.contains(MANAGED_MARKER),
                "{} is missing the managed ownership marker",
                skill.name
            );
        }
    }

    #[test]
    fn managed_skill_payloads_use_lf_line_endings() {
        for skill in MANAGED_SKILLS {
            assert!(
                !skill.content.contains('\r'),
                "{} must use LF line endings",
                skill.name
            );
            assert!(
                skill.content.ends_with('\n'),
                "{} must end with a newline",
                skill.name
            );
        }
    }

    #[test]
    fn relative_paths_are_safe_relative_skill_markdown_files() {
        for skill in MANAGED_SKILLS {
            let expected_suffix = format!("{}/SKILL.md", skill.name);
            assert_eq!(skill.relative_path, expected_suffix);
            assert!(
                !skill.name.contains(['/', '\\']),
                "{} must be a single skill directory name",
                skill.name
            );

            let path = Path::new(skill.relative_path);
            assert!(
                !path.is_absolute(),
                "{} relative_path must not be absolute",
                skill.name
            );
            let components: Vec<_> = path.components().collect();
            assert_eq!(
                components,
                vec![
                    Component::Normal(OsStr::new(skill.name)),
                    Component::Normal(OsStr::new("SKILL.md")),
                ],
                "{} relative_path must be exactly <skill>/SKILL.md with no parent/current/root components",
                skill.name
            );
        }
    }

    #[test]
    fn every_routing_tool_appears_only_in_its_intended_cluster() {
        let expected_by_tool: BTreeMap<_, _> = EXPECTED_TOOL_CLUSTERS.iter().copied().collect();
        assert_eq!(expected_by_tool.len(), EXPECTED_TOOL_CLUSTERS.len());

        for (tool, expected_skill) in expected_by_tool {
            let containing_skills: Vec<_> = MANAGED_SKILLS
                .iter()
                .filter(|skill| skill.content.contains(tool))
                .map(|skill| skill.name)
                .collect();

            assert_eq!(
                containing_skills,
                vec![expected_skill],
                "{tool} should appear in exactly one intended skill"
            );
        }
    }

    #[test]
    fn project_scoped_skills_share_the_static_client_contract() {
        for skill_name in PROJECT_SCOPED_SKILLS {
            let skill = MANAGED_SKILLS
                .iter()
                .find(|skill| skill.name == *skill_name)
                .unwrap_or_else(|| panic!("missing managed skill {skill_name}"));
            for required in [
                "Session-aware MCP clients",
                "Static MCP clients",
                "must pass `workspace` and `project` together on every project-scoped call",
                "nearest `.ai-memory.toml`",
                "normalized `upstream`, then `origin`",
                "folder basename only when no valid remote exists",
                "Never rely on the server's last active project",
                "`global=true` must omit `workspace`, `project`, and `scopes`",
                "`scope: \"global\"`",
            ] {
                assert!(
                    skill.content.contains(required),
                    "{skill_name} is missing scope guidance: {required}"
                );
            }
            for contradictory in [
                "Pass workspace and project together only when",
                "Never pass scope arguments",
                "omit project, workspace, and cwd arguments unless",
            ] {
                assert!(
                    !skill.content.contains(contradictory),
                    "{skill_name} contains contradictory scope guidance: {contradictory}"
                );
            }
        }
    }

    fn parse_frontmatter(skill: &ManagedSkill) -> Frontmatter {
        let Some(rest) = skill.content.strip_prefix("---\n") else {
            panic!("{} must start with frontmatter", skill.name);
        };
        let Some((frontmatter, _body)) = rest.split_once("\n---\n") else {
            panic!("{} must close frontmatter", skill.name);
        };
        serde_saphyr::from_str(frontmatter)
            .unwrap_or_else(|e| panic!("{} frontmatter must be valid YAML: {e}", skill.name))
    }

    fn directory_name(skill: &ManagedSkill) -> &str {
        skill
            .relative_path
            .split('/')
            .next()
            .unwrap_or_else(|| panic!("{} has an empty relative path", skill.name))
    }

    #[test]
    fn grok_handoff_rule_is_managed_and_mandates_first_turn_list() {
        assert_eq!(GROK_RULES_DIR, "rules");
        assert_eq!(GROK_HANDOFF_RULE_FILENAME, "ai-memory-handoff.md");
        assert!(!GROK_HANDOFF_RULE_MARKER.is_empty());
        assert!(GROK_HANDOFF_RULE_CONTENT.contains(GROK_HANDOFF_RULE_MARKER));
        assert!(!GROK_HANDOFF_RULE_CONTENT.contains('\r'));
        assert!(GROK_HANDOFF_RULE_CONTENT.ends_with('\n'));
        assert!(GROK_HANDOFF_RULE_CONTENT.contains("memory_handoff_list"));
        assert!(GROK_HANDOFF_RULE_CONTENT.contains("first tool"));
        assert!(GROK_HANDOFF_RULE_CONTENT.contains("static MCP client"));
        assert!(GROK_HANDOFF_RULE_CONTENT.contains("`workspace` and `project` together"));
        let list_at = GROK_HANDOFF_RULE_CONTENT
            .find("memory_handoff_list")
            .expect("list tool");
        let accept_at = GROK_HANDOFF_RULE_CONTENT
            .find("memory_handoff_accept")
            .expect("accept tool named so the rule can forbid it as first");
        assert!(
            list_at < accept_at,
            "the rule must mandate list before it mentions accept"
        );
        assert!(
            GROK_HANDOFF_RULE_CONTENT
                .contains("Do not call `memory_handoff_accept` as the first tool")
        );
        assert!(GROK_HANDOFF_RULE_CONTENT.contains("empty `handoffs` array"));
        // A guessed project that does not exist answers with a scope error,
        // not an empty list; the correction must fire on that too.
        assert!(GROK_HANDOFF_RULE_CONTENT.contains("or a scope error"));
        // The baton PostToolUse claimed is authoritative over listed rows.
        assert!(GROK_HANDOFF_RULE_CONTENT.contains("it wins"));
    }

    #[test]
    fn grok_handoff_rule_names_the_use_tool_invocation_before_search_tool() {
        assert!(GROK_HANDOFF_RULE_CONTENT.contains("use_tool"));
        let qualified_at = GROK_HANDOFF_RULE_CONTENT
            .find("ai-memory__memory_handoff_list")
            .expect("the rule must hand Grok the qualified catalog key, not the bare tool name");
        if let Some(search_at) = GROK_HANDOFF_RULE_CONTENT.find("search_tool") {
            assert!(
                qualified_at < search_at,
                "the key must precede search_tool so the rule never reads as a discovery roundtrip"
            );
        }
        assert!(
            GROK_HANDOFF_RULE_CONTENT.contains("do not search the filesystem"),
            "scope resolution must stay inside the current repository"
        );
    }

    #[test]
    fn grok_handoff_rule_lists_before_it_reads_the_marker() {
        let list_at = GROK_HANDOFF_RULE_CONTENT
            .find("ai-memory__memory_handoff_list")
            .expect("the rule must hand Grok the qualified catalog key");
        let marker_at = GROK_HANDOFF_RULE_CONTENT
            .find("./.ai-memory.toml")
            .expect("scope correction must name the marker it reads");
        assert!(
            list_at < marker_at,
            "the list call must come before the marker read, or the read becomes the first tool \
             call and PostToolUse claims the baton before the rule's own step 1 runs"
        );
        assert!(
            GROK_HANDOFF_RULE_CONTENT.contains("before reading any file"),
            "step 1 must say the list precedes any file read"
        );
    }

    #[test]
    fn grok_handoff_rule_does_not_embed_a_handoff_peek() {
        for leak in [
            "📥",
            "Previous session",
            "open_questions",
            "files_touched",
            "next_steps",
            "You were working on",
        ] {
            assert!(
                !GROK_HANDOFF_RULE_CONTENT.contains(leak),
                "Grok rule must not embed a canned handoff peek; found {leak:?}"
            );
        }
        assert!(!GROK_HANDOFF_RULE_CONTENT.contains(MANAGED_MARKER));
    }
}
