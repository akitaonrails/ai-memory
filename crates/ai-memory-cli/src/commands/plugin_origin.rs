//! Hook events launched by an ai-memory marketplace plugin (#1166).
//!
//! The plugin's launcher (`plugins/<harness>/scripts/ai-memory-hook`) hands
//! every lifecycle event to `ai-memory hook` with `AI_MEMORY_HOOK_ORIGIN` set
//! to `<agent>-plugin`. Everything else about the event (capture policy,
//! marker scope, server profiles, spooling) is the ordinary native hook path;
//! this module only decides whether a plugin-launched event is dropped before
//! that path starts:
//!
//! - **De-duplication.** A harness runs a plugin's hook and an identical
//!   settings-file hook separately, so a user who installed both would be
//!   captured twice. The settings copy wins (it carries flags the plugin does
//!   not, such as `--capture-assistant` or a baked `--project-strategy`); the
//!   plugin copy of an event the settings hooks already deliver is dropped.
//! - **Folderless sessions.** A session with no project folder (no `cwd`, or a
//!   Claude desktop scratch workspace) is not captured through the plugin.
//!
//! An environment variable rather than a flag keeps an older binary working
//! with a newer plugin: it ignores the variable and only skips these checks.

use std::path::{Component, Path, PathBuf};

use super::install_hooks::{self, HooksFileShape};
use super::render_shared::{CLAUDE_CODE_EVENTS, DROID_EVENTS, MUSE_EVENTS};

/// The environment variable the plugin launcher sets.
pub(crate) const HOOK_ORIGIN_ENV: &str = "AI_MEMORY_HOOK_ORIGIN";

/// Which plugin launched an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginOrigin {
    /// The Claude Code plugin (`plugins/claude-code`).
    ClaudeCode,
    /// The Codex plugin (`plugins/codex`).
    Codex,
    /// The Claude Code plugin loaded by Factory Droid through its Claude
    /// plugin compatibility (the launcher re-attributes it to `droid`).
    Droid,
    /// The Claude Code plugin loaded by Meta Muse Code, which reads
    /// `.claude-plugin/plugin.json` plugins (the launcher re-attributes it to
    /// `muse`).
    Muse,
}

impl PluginOrigin {
    /// The origin `lookup` reports, if any. Unknown values are not a plugin:
    /// the event takes the ordinary path.
    pub(crate) fn from_env(mut lookup: impl FnMut(&str) -> Option<String>) -> Option<Self> {
        match lookup(HOOK_ORIGIN_ENV)?.trim() {
            "claude-code-plugin" => Some(Self::ClaudeCode),
            "codex-plugin" => Some(Self::Codex),
            "droid-plugin" => Some(Self::Droid),
            "muse-plugin" => Some(Self::Muse),
            _ => None,
        }
    }
}

/// A file whose ai-memory hooks outrank the plugin, and where its event map
/// sits.
pub(crate) type SettingsHooks = (PathBuf, HooksFileShape);

/// Whether a plugin-launched event must be dropped before capture. `event` is
/// the wire event the launcher passed and `harness_event` the payload's own
/// `hook_event_name` (see [`settings_event_name`]); `cwd` is the payload's
/// working directory; `settings_files` are the files whose ai-memory hooks
/// outrank the plugin (see [`settings_files_for`]).
pub(crate) fn drops_plugin_event(
    origin: PluginOrigin,
    event: &str,
    harness_event: Option<&str>,
    cwd: Option<&str>,
    settings_files: &[SettingsHooks],
) -> bool {
    if is_folderless(cwd) {
        return true;
    }
    let Some(name) = settings_event_name(origin, event, harness_event) else {
        // Muse's subagent events come from its built-in reminder observers,
        // each under its own session id, every turn; `install-hooks --agent
        // muse` leaves them out, and so does the plugin.
        return origin == PluginOrigin::Muse;
    };
    settings_files
        .iter()
        .any(|(path, shape)| install_hooks::hooks_file_delivers_ai_memory_event(path, *shape, name))
}

/// The settings files that outrank the plugin for `origin`.
///
/// Claude Code: the user `settings.json` (honouring `CLAUDE_CONFIG_DIR`) and
/// the checkout's `.claude/settings.local.json`, the two files
/// `install-hooks` writes. The checkout is `CLAUDE_PROJECT_DIR` when Claude
/// set it, else the payload's `cwd`.
///
/// Codex: none. Codex silently skips a settings hook the user has not
/// trusted, and nothing at hook time can tell a trusted one from a skipped
/// one, so dropping the plugin copy could lose the event entirely. `setup`
/// and `doctor` handle a double Codex install instead.
///
/// Droid: the one file Droid reads hooks from, as `install-hooks --agent
/// droid` decides it. `~/.factory/hooks.json` (the bare event map) when it
/// exists; otherwise the `hooks` key of `~/.factory/settings.json`, which
/// Droid reads only while `hooks.json` is absent. Checking a file Droid
/// ignores could drop the plugin copy of an event nothing else delivers.
///
/// Muse: the `hooks` key of `${XDG_CONFIG_HOME:-~/.config}/muse/settings.json`,
/// the one file `install-hooks --agent muse` writes. Muse runs hooks with a
/// cleared environment (`HOME`, `PATH`, `PWD` and its own `MUSE_*`), so an
/// `XDG_CONFIG_HOME` Muse itself honoured is usually not visible here and the
/// default location is checked; the plugin copy is then kept, never lost.
pub(crate) fn settings_files_for(
    origin: PluginOrigin,
    cwd: Option<&str>,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Vec<SettingsHooks> {
    match origin {
        PluginOrigin::Codex => Vec::new(),
        PluginOrigin::ClaudeCode => {
            let mut files = Vec::with_capacity(2);
            let config_dir = lookup("CLAUDE_CONFIG_DIR").map(std::ffi::OsString::from);
            if let Ok(user) = install_hooks::claude_settings_path_in(config_dir) {
                files.push((user, HooksFileShape::HooksKey));
            }
            let project = lookup("CLAUDE_PROJECT_DIR")
                .filter(|dir| !dir.trim().is_empty())
                .or_else(|| cwd.map(str::to_owned));
            if let Some(project) = project {
                files.push((
                    install_hooks::project_claude_settings_local(Path::new(&project)),
                    HooksFileShape::HooksKey,
                ));
            }
            files
        }
        PluginOrigin::Droid => {
            // `AI_MEMORY_HOME` relocates every agent home ai-memory resolves
            // (see `path_util::home_dir`); read it through the lookup so the
            // check follows the same root `install-hooks` wrote under.
            let hooks_json = match lookup("AI_MEMORY_HOME").filter(|home| !home.trim().is_empty()) {
                Some(home) => {
                    Some(super::install_mcp::factory_home(Path::new(&home)).join("hooks.json"))
                }
                None => install_hooks::droid_hooks_path().ok(),
            };
            let Some(hooks_json) = hooks_json else {
                return Vec::new();
            };
            if hooks_json.exists() {
                vec![(hooks_json, HooksFileShape::EventMap)]
            } else {
                vec![(
                    hooks_json.with_file_name("settings.json"),
                    HooksFileShape::HooksKey,
                )]
            }
        }
        PluginOrigin::Muse => {
            let home = lookup("AI_MEMORY_HOME")
                .filter(|home| !home.trim().is_empty())
                .map(PathBuf::from)
                .or_else(super::path_util::home_dir);
            let xdg = lookup("XDG_CONFIG_HOME").map(std::ffi::OsString::from);
            super::install_mcp::muse_settings_path_in(xdg, home)
                .map(|settings| vec![(settings, HooksFileShape::HooksKey)])
                .unwrap_or_default()
        }
    }
}

/// The settings-file event name for a wire event (`session-start` →
/// `SessionStart`).
///
/// Two settings events can share one wire event (`PostToolUse` and
/// `PostToolUseFailure` both run `post-tool-use`). The harness's own event
/// name picks between them; otherwise the first entry wins. So settings that
/// wire only the success event do not silence the plugin's failure copy.
fn settings_event_name(
    origin: PluginOrigin,
    event: &str,
    harness_event: Option<&str>,
) -> Option<&'static str> {
    let events: &[(&'static str, &'static str)] = match origin {
        PluginOrigin::ClaudeCode => &CLAUDE_CODE_EVENTS,
        PluginOrigin::Droid => &DROID_EVENTS,
        PluginOrigin::Muse => &MUSE_EVENTS,
        PluginOrigin::Codex => return None,
    };
    let names = || {
        events
            .iter()
            .filter(|(_, script)| script.strip_suffix(".sh") == Some(event))
            .map(|(name, _)| *name)
    };
    let first = names().next()?;
    Some(
        names()
            .find(|name| Some(*name) == harness_event)
            .unwrap_or(first),
    )
}

/// A session with no project folder: no `cwd` at all, or one inside a Claude
/// desktop scratch workspace (`<Claude app dir>/scratch-workspaces/...`,
/// created for a Code-tab session started without a folder). Checked
/// lexically: canonicalizing diverges from the raw runtime path on macOS and
/// Windows.
fn is_folderless(cwd: Option<&str>) -> bool {
    let Some(cwd) = cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) else {
        return true;
    };
    let normalized = cwd.replace('\\', "/");
    let components: Vec<&str> = Path::new(&normalized)
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect();
    components
        .windows(2)
        .any(|pair| pair[0].eq_ignore_ascii_case("claude") && pair[1] == "scratch-workspaces")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> impl FnMut(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    const SETTINGS_HOOK: &str = r#"{"hooks":{"SessionStart":[{"matcher":"","hooks":[{"type":"command","command":"/usr/bin/ai-memory hook --event session-start --agent claude-code --server-url http://127.0.0.1:49374"}]}]}}"#;

    #[test]
    fn origin_parses_only_known_plugins() {
        assert_eq!(
            PluginOrigin::from_env(lookup(&[(HOOK_ORIGIN_ENV, "claude-code-plugin")])),
            Some(PluginOrigin::ClaudeCode)
        );
        assert_eq!(
            PluginOrigin::from_env(lookup(&[(HOOK_ORIGIN_ENV, " codex-plugin\n")])),
            Some(PluginOrigin::Codex)
        );
        assert_eq!(
            PluginOrigin::from_env(lookup(&[(HOOK_ORIGIN_ENV, "droid-plugin")])),
            Some(PluginOrigin::Droid)
        );
        assert_eq!(
            PluginOrigin::from_env(lookup(&[(HOOK_ORIGIN_ENV, "muse-plugin")])),
            Some(PluginOrigin::Muse)
        );
        for other in ["", "claude-code", "cursor-plugin", "settings"] {
            assert_eq!(
                PluginOrigin::from_env(lookup(&[(HOOK_ORIGIN_ENV, other)])),
                None,
                "{other:?}"
            );
        }
        assert_eq!(PluginOrigin::from_env(lookup(&[])), None);
    }

    /// Adversarial: with an ai-memory settings hook for the same event the
    /// plugin copy is dropped. Controls: another event, a third-party hook,
    /// and no settings file at all keep the plugin copy.
    #[test]
    fn plugin_copy_is_dropped_only_when_settings_deliver_the_same_event() {
        let temp = tempfile::tempdir().unwrap();
        let settings = temp.path().join("settings.json");
        std::fs::write(&settings, SETTINGS_HOOK).unwrap();
        let cwd = Some("/work/repo");
        let files = vec![(settings.clone(), HooksFileShape::HooksKey)];

        assert!(drops_plugin_event(
            PluginOrigin::ClaudeCode,
            "session-start",
            None,
            cwd,
            &files
        ));
        assert!(
            !drops_plugin_event(PluginOrigin::ClaudeCode, "stop", None, cwd, &files),
            "an event the settings hooks do not wire stays with the plugin"
        );
        assert!(!drops_plugin_event(
            PluginOrigin::ClaudeCode,
            "session-start",
            None,
            cwd,
            &[(temp.path().join("missing.json"), HooksFileShape::HooksKey)]
        ));

        std::fs::write(
            &settings,
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"node other-tool.mjs"}]}]}}"#,
        )
        .unwrap();
        assert!(!drops_plugin_event(
            PluginOrigin::ClaudeCode,
            "session-start",
            None,
            cwd,
            &files
        ));

        std::fs::write(&settings, "{ not json").unwrap();
        assert!(!drops_plugin_event(
            PluginOrigin::ClaudeCode,
            "session-start",
            None,
            cwd,
            &files
        ));
    }

    /// Adversarial: settings that wire only `PostToolUse` must not drop the
    /// plugin's `PostToolUseFailure` copy. Controls: the success copy, a
    /// payload without a harness event, and settings wiring both are dropped.
    #[test]
    fn a_plugin_failure_copy_is_dropped_only_when_settings_deliver_the_failure_event() {
        let temp = tempfile::tempdir().unwrap();
        let settings = temp.path().join("settings.json");
        let cwd = Some("/work/repo");
        let files = vec![(settings.clone(), HooksFileShape::HooksKey)];
        let entry = |event: &str| {
            format!(
                r#""{event}":[{{"matcher":"","hooks":[{{"type":"command","command":"/usr/bin/ai-memory hook --event post-tool-use --agent claude-code --server-url http://127.0.0.1:49374"}}]}}]"#
            )
        };
        std::fs::write(
            &settings,
            format!(r#"{{"hooks":{{{}}}}}"#, entry("PostToolUse")),
        )
        .unwrap();
        assert!(
            !drops_plugin_event(
                PluginOrigin::ClaudeCode,
                "post-tool-use",
                Some("PostToolUseFailure"),
                cwd,
                &files
            ),
            "settings deliver only the success event: the plugin keeps the failure"
        );
        assert!(drops_plugin_event(
            PluginOrigin::ClaudeCode,
            "post-tool-use",
            Some("PostToolUse"),
            cwd,
            &files
        ));
        assert!(drops_plugin_event(
            PluginOrigin::ClaudeCode,
            "post-tool-use",
            None,
            cwd,
            &files
        ));
        // An unrelated harness event falls back to the first mapping.
        assert!(drops_plugin_event(
            PluginOrigin::ClaudeCode,
            "post-tool-use",
            Some("SessionStart"),
            cwd,
            &files
        ));

        std::fs::write(
            &settings,
            format!(
                r#"{{"hooks":{{{},{}}}}}"#,
                entry("PostToolUse"),
                entry("PostToolUseFailure")
            ),
        )
        .unwrap();
        assert!(drops_plugin_event(
            PluginOrigin::ClaudeCode,
            "post-tool-use",
            Some("PostToolUseFailure"),
            cwd,
            &files
        ));
    }

    /// Adversarial: a settings hook narrowed by a matcher (one tool) must not
    /// suppress the plugin copy for every other tool. Controls: an empty,
    /// `*` or absent matcher catches everything and drops the plugin copy.
    #[test]
    fn only_a_catch_all_matcher_suppresses_the_plugin_copy() {
        let temp = tempfile::tempdir().unwrap();
        let settings = temp.path().join("settings.json");
        let files = vec![(settings.clone(), HooksFileShape::HooksKey)];
        let hook = |matcher: Option<serde_json::Value>| {
            let mut entry = serde_json::json!({"hooks": [{"type": "command", "command": "/usr/bin/ai-memory hook --event post-tool-use --agent claude-code --server-url http://h"}]});
            if let Some(matcher) = matcher {
                entry["matcher"] = matcher;
            }
            serde_json::json!({"hooks": {"PostToolUse": [entry]}}).to_string()
        };
        let drops = || {
            drops_plugin_event(
                PluginOrigin::ClaudeCode,
                "post-tool-use",
                None,
                Some("/work/repo"),
                &files,
            )
        };
        for narrowed in ["Bash", "Edit|Write", "mcp__.*"] {
            std::fs::write(&settings, hook(Some(serde_json::json!(narrowed)))).unwrap();
            assert!(!drops(), "{narrowed}");
        }
        for catch_all in [
            Some(serde_json::json!("")),
            Some(serde_json::json!("*")),
            None,
        ] {
            std::fs::write(&settings, hook(catch_all.clone())).unwrap();
            assert!(drops(), "{catch_all:?}");
        }
    }

    /// Codex settings hooks never suppress the plugin: an untrusted one would
    /// be skipped by Codex and the event lost.
    #[test]
    fn codex_plugin_events_are_never_deduplicated_against_settings() {
        let temp = tempfile::tempdir().unwrap();
        let settings = temp.path().join("hooks.json");
        std::fs::write(&settings, SETTINGS_HOOK).unwrap();
        assert!(!drops_plugin_event(
            PluginOrigin::Codex,
            "session-start",
            None,
            Some("/work/repo"),
            &[(settings, HooksFileShape::HooksKey)]
        ));
        assert!(settings_files_for(PluginOrigin::Codex, Some("/w"), lookup(&[])).is_empty());
    }

    const DROID_EVENT_MAP: &str = r#"{"SessionStart":[{"matcher":"","hooks":[{"type":"command","command":"/usr/bin/ai-memory hook --event session-start --agent droid --server-url http://127.0.0.1:49374"}]}]}"#;

    fn droid_files(home: &Path) -> Vec<SettingsHooks> {
        let home = home.to_string_lossy().into_owned();
        settings_files_for(
            PluginOrigin::Droid,
            Some("/work/repo"),
            lookup(&[("AI_MEMORY_HOME", home.as_str())]),
        )
    }

    fn droid_drops(home: &Path, event: &str) -> bool {
        drops_plugin_event(
            PluginOrigin::Droid,
            event,
            None,
            Some("/work/repo"),
            &droid_files(home),
        )
    }

    /// Adversarial: Droid running the Claude Code plugin next to an
    /// `install-hooks --agent droid` install delivers each event twice unless
    /// the plugin copy is dropped. Controls: no Droid hooks at all, and an
    /// event the Droid hooks do not wire, keep the plugin copy.
    #[test]
    fn droid_plugin_copy_is_dropped_when_droid_hooks_deliver_the_event() {
        let temp = tempfile::tempdir().unwrap();
        let factory = temp.path().join(".factory");
        std::fs::create_dir_all(&factory).unwrap();
        assert!(
            !droid_drops(temp.path(), "session-start"),
            "nothing installed"
        );

        std::fs::write(factory.join("hooks.json"), DROID_EVENT_MAP).unwrap();
        assert!(droid_drops(temp.path(), "session-start"));
        assert!(
            !droid_drops(temp.path(), "stop"),
            "an event hooks.json does not wire stays with the plugin"
        );
        assert!(
            !droid_drops(temp.path(), "subagent-start"),
            "Droid has no subagent hook, so the plugin keeps it"
        );
    }

    /// Droid reads `settings.json` hooks only while `hooks.json` is absent, so
    /// that is the only time they may suppress the plugin. Adversarial: an
    /// ai-memory hook left in `settings.json` beside a `hooks.json` without
    /// one is dead config and must not drop the plugin copy.
    #[test]
    fn droid_settings_hooks_count_only_while_hooks_json_is_absent() {
        let temp = tempfile::tempdir().unwrap();
        let factory = temp.path().join(".factory");
        std::fs::create_dir_all(&factory).unwrap();
        std::fs::write(
            factory.join("settings.json"),
            format!(r#"{{"model":"x","hooks":{DROID_EVENT_MAP}}}"#),
        )
        .unwrap();
        assert!(droid_drops(temp.path(), "session-start"));

        std::fs::write(factory.join("hooks.json"), "{}").unwrap();
        assert!(!droid_drops(temp.path(), "session-start"));
    }

    #[test]
    fn droid_plugin_skips_folderless_sessions() {
        assert!(drops_plugin_event(
            PluginOrigin::Droid,
            "user-prompt-submit",
            None,
            None,
            &[]
        ));
        assert!(!drops_plugin_event(
            PluginOrigin::Droid,
            "user-prompt-submit",
            None,
            Some("/work/repo"),
            &[]
        ));
    }

    const MUSE_SETTINGS: &str = r#"{"schema_version":1,"hooks":{"SessionStart":[{"matcher":"","hooks":[{"type":"command","command":"/usr/bin/ai-memory hook --event session-start --agent muse --server-url http://127.0.0.1:49374","timeout":30}]}],"Stop":[{"matcher":"write_file","hooks":[{"type":"command","command":"/usr/bin/ai-memory hook --event stop --agent muse --server-url http://127.0.0.1:49374"}]}]}}"#;

    fn muse_drops(home: &Path, xdg: Option<&Path>, event: &str) -> bool {
        let home = home.to_string_lossy().into_owned();
        let xdg = xdg.map(|dir| dir.to_string_lossy().into_owned());
        let mut pairs = vec![("AI_MEMORY_HOME", home.as_str())];
        if let Some(xdg) = &xdg {
            pairs.push(("XDG_CONFIG_HOME", xdg.as_str()));
        }
        let files = settings_files_for(PluginOrigin::Muse, Some("/work/repo"), lookup(&pairs));
        drops_plugin_event(PluginOrigin::Muse, event, None, Some("/work/repo"), &files)
    }

    /// Adversarial: Muse running the Claude Code plugin next to an
    /// `install-hooks --agent muse` install delivers each event twice unless
    /// the plugin copy is dropped. Only a catch-all matcher counts: a hook
    /// narrowed to one tool does not deliver every occurrence. Controls: no
    /// Muse settings at all, and an event the settings do not wire, keep the
    /// plugin copy. The plugin's subagent events are always dropped, as
    /// `install-hooks --agent muse` never wires them.
    #[test]
    fn muse_plugin_copy_is_dropped_when_muse_settings_hooks_deliver_the_event() {
        let temp = tempfile::tempdir().unwrap();
        let muse = temp.path().join(".config").join("muse");
        std::fs::create_dir_all(&muse).unwrap();
        assert!(
            !muse_drops(temp.path(), None, "session-start"),
            "nothing installed"
        );

        std::fs::write(muse.join("settings.json"), MUSE_SETTINGS).unwrap();
        assert!(muse_drops(temp.path(), None, "session-start"));
        assert!(
            !muse_drops(temp.path(), None, "stop"),
            "a Stop hook narrowed by a matcher does not deliver every event"
        );
        assert!(
            !muse_drops(temp.path(), None, "user-prompt-submit"),
            "an event settings.json does not wire stays with the plugin"
        );
        for event in ["subagent-start", "subagent-stop"] {
            assert!(muse_drops(temp.path(), None, event), "{event}");
        }
        assert!(
            !drops_plugin_event(
                PluginOrigin::ClaudeCode,
                "subagent-start",
                None,
                Some("/work/repo"),
                &[]
            ),
            "control: Claude Code keeps its subagent events"
        );
    }

    /// When the hook can see `XDG_CONFIG_HOME`, the settings file under it is
    /// the one checked, and the default location no longer counts.
    #[test]
    fn muse_settings_follow_a_visible_xdg_config_home() {
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        std::fs::create_dir_all(xdg.join("muse")).unwrap();
        std::fs::write(xdg.join("muse").join("settings.json"), MUSE_SETTINGS).unwrap();
        assert!(muse_drops(temp.path(), Some(&xdg), "session-start"));
        assert!(!muse_drops(temp.path(), None, "session-start"));
    }

    #[test]
    fn muse_plugin_skips_folderless_sessions() {
        assert!(drops_plugin_event(
            PluginOrigin::Muse,
            "user-prompt-submit",
            None,
            None,
            &[]
        ));
        assert!(!drops_plugin_event(
            PluginOrigin::Muse,
            "user-prompt-submit",
            None,
            Some("/work/repo"),
            &[]
        ));
    }

    #[test]
    fn settings_files_honour_claude_config_dir_and_project_dir() {
        let files = settings_files_for(
            PluginOrigin::ClaudeCode,
            Some("/payload/cwd"),
            lookup(&[
                ("CLAUDE_CONFIG_DIR", "/relocated/claude"),
                ("CLAUDE_PROJECT_DIR", "/the/project"),
            ]),
        );
        assert_eq!(
            files[0],
            (
                Path::new("/relocated/claude").join("settings.json"),
                HooksFileShape::HooksKey
            )
        );
        assert!(
            files[1]
                .0
                .ends_with(Path::new(".claude").join("settings.local.json")),
            "{files:?}"
        );
        assert!(
            files[1].0.starts_with("/the/project"),
            "CLAUDE_PROJECT_DIR outranks the payload cwd: {files:?}"
        );

        let files = settings_files_for(
            PluginOrigin::ClaudeCode,
            Some("/payload/cwd"),
            lookup(&[("CLAUDE_PROJECT_DIR", " ")]),
        );
        assert!(files[1].0.starts_with("/payload/cwd"), "{files:?}");
    }

    #[test]
    fn folderless_sessions_are_dropped() {
        for cwd in [
            None,
            Some(""),
            Some("  "),
            Some("/home/me/.config/Claude/scratch-workspaces/acct/org/scratch-2026-10-13-ab12"),
            Some("/Users/me/Library/Application Support/Claude/scratch-workspaces/a/b/scratch-1"),
            Some(r"C:\Users\me\AppData\Roaming\Claude\scratch-workspaces\a\b\scratch-1"),
        ] {
            assert!(
                drops_plugin_event(
                    PluginOrigin::ClaudeCode,
                    "user-prompt-submit",
                    None,
                    cwd,
                    &[]
                ),
                "{cwd:?}"
            );
        }
        for cwd in [
            "/home/me/src/claude",
            "/home/me/src/scratch-workspaces",
            "/home/me/Claude/projects/app",
        ] {
            assert!(
                !drops_plugin_event(
                    PluginOrigin::ClaudeCode,
                    "user-prompt-submit",
                    None,
                    Some(cwd),
                    &[]
                ),
                "{cwd}"
            );
        }
    }
}
