//! Hermes Agent hook consent, per profile.
//!
//! Hermes runs a shell hook from `config.yaml` only when the exact
//! `(event, command)` pair is in that home's `shell-hooks-allowlist.json`,
//! unless `hooks_auto_accept` (or `--accept-hooks` / `HERMES_ACCEPT_HOOKS` on
//! the launch) approves it. A gateway has no TTY to ask on, so an
//! unapproved hook is skipped with one log line per event and nothing is
//! captured. Every profile under `<root>/profiles/<name>` is its own home
//! with its own allowlist, so wiring the hooks into a profile's config does
//! not make them run there.
//!
//! The rules mirror `agent/shell_hooks.py` and `hermes_constants.py` in
//! Hermes Agent (<https://github.com/NousResearch/hermes-agent>); hook
//! consent is documented at
//! <https://hermes-agent.nousresearch.com/docs/user-guide/features/hooks>.
//! This only reads; approving a hook stays Hermes' own prompt.

use std::path::{Path, PathBuf};

use serde::Serialize;

const ALLOWLIST_FILENAME: &str = "shell-hooks-allowlist.json";

/// What `doctor` found in one Hermes home.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct HermesProfileReport {
    /// `default` for the root home, else the directory name under `profiles/`.
    pub(crate) profile: String,
    pub(crate) home: String,
    /// Events whose `hooks:` entries run `ai-memory hook --agent hermes`.
    pub(crate) hook_events: Vec<String>,
    /// Of those, the events whose exact command has no allowlist approval.
    /// Hermes skips them unless `auto_accept` is set. Commands are never
    /// reported: they can carry a bearer token.
    pub(crate) not_allowlisted: Vec<String>,
    /// `hooks_auto_accept` is truthy, so Hermes approves every hook itself.
    pub(crate) auto_accept: bool,
}

/// The Hermes root, as Hermes resolves it: `$HERMES_HOME`, or the platform
/// default (`%LOCALAPPDATA%\hermes` on Windows, `~/.hermes` elsewhere; see
/// [`crate::commands::path_util::hermes_home`]). A `HERMES_HOME` that points
/// at a profile (`<root>/profiles/<name>`) is walked back up to its root, so
/// every profile is still found.
pub(crate) fn hermes_root(env: &crate::config::RuntimeEnv, home: Option<&Path>) -> Option<PathBuf> {
    hermes_root_from(crate::commands::path_util::hermes_home(env, home)?)
}

/// The root that holds `home`: its parent when `home` is `<root>/profiles/<name>`.
fn hermes_root_from(home: PathBuf) -> Option<PathBuf> {
    match home.parent() {
        Some(parent) if parent.file_name().is_some_and(|name| name == "profiles") => {
            parent.parent().map(Path::to_path_buf)
        }
        _ => Some(home),
    }
}

/// One report per Hermes home under `root` that has a `config.yaml`: the root
/// first, then its profiles by name. Empty when Hermes is not installed.
pub(crate) fn reports(root: &Path) -> Vec<HermesProfileReport> {
    let mut homes = vec![("default".to_owned(), root.to_path_buf())];
    if let Ok(entries) = std::fs::read_dir(root.join("profiles")) {
        let mut profiles: Vec<(String, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                crate::commands::path_util::is_hermes_profile_id(&name)
                    .then(|| (name, entry.path()))
            })
            .collect();
        profiles.sort();
        homes.extend(profiles);
    }
    homes
        .into_iter()
        .filter_map(|(profile, home)| profile_report(profile, &home))
        .collect()
}

fn profile_report(profile: String, home: &Path) -> Option<HermesProfileReport> {
    let config = std::fs::read_to_string(home.join("config.yaml")).ok()?;
    let hooks = ai_memory_hooks(&config);
    let approved = approvals(home);
    let mut hook_events = Vec::new();
    let mut not_allowlisted = Vec::new();
    for (event, command) in &hooks {
        if !hook_events.contains(event) {
            hook_events.push(event.clone());
        }
        if !approved.contains(&(event.clone(), command.clone())) && !not_allowlisted.contains(event)
        {
            not_allowlisted.push(event.clone());
        }
    }
    Some(HermesProfileReport {
        profile,
        home: home.display().to_string(),
        hook_events,
        not_allowlisted,
        auto_accept: auto_accept(&config),
    })
}

/// The `(event, command)` pairs approved in `home`'s allowlist. A missing or
/// malformed file approves nothing, as in Hermes.
fn approvals(home: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(home.join(ALLOWLIST_FILENAME)) else {
        return Vec::new();
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    root.get("approvals")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some((
                entry.get("event")?.as_str()?.to_owned(),
                entry.get("command")?.as_str()?.to_owned(),
            ))
        })
        .collect()
}

/// `hooks_auto_accept` at the top level of `config.yaml`. Hermes takes a YAML
/// boolean as is and a string when it reads as true.
fn auto_accept(config: &str) -> bool {
    config.lines().any(|line| {
        line.strip_prefix("hooks_auto_accept:")
            .is_some_and(|value| {
                let value = strip_comment(value).trim();
                let unquoted = value.trim_matches(|c| c == '\'' || c == '"');
                matches!(
                    unquoted.to_ascii_lowercase().as_str(),
                    "true" | "yes" | "on"
                ) || (unquoted == "1" && unquoted.len() < value.len())
            })
    })
}

/// The `(event, command)` pairs of the top-level `hooks:` block whose command
/// runs `ai-memory hook --agent hermes`, with each command stripped as
/// Hermes strips it before matching the allowlist.
///
/// A narrow reader for the block style `install-hooks --agent hermes` prints
/// and PyYAML writes back (plain, single- and double-quoted scalars, folded
/// over several lines); the workspace carries no YAML parser. A flow-style
/// `hooks: {...}` reads as no hooks.
fn ai_memory_hooks(config: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = config.lines().collect();
    let mut hooks = Vec::new();
    let mut in_hooks = false;
    let mut event_indent = None;
    let mut event = None;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        index += 1;
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - trimmed.len();
        if indent == 0 {
            in_hooks = strip_comment(trimmed).trim_end() == "hooks:";
            event_indent = None;
            event = None;
            continue;
        }
        if !in_hooks {
            continue;
        }
        let event_indent = *event_indent.get_or_insert(indent);
        if indent <= event_indent {
            event = strip_comment(trimmed)
                .trim_end()
                .strip_suffix(':')
                .map(str::to_owned);
            continue;
        }
        let entry = trimmed.strip_prefix('-').map_or(trimmed, str::trim_start);
        let (Some(value), Some(event)) = (entry.strip_prefix("command:"), event.as_ref()) else {
            continue;
        };
        let (command, consumed) = scalar(value, &lines[index..], indent);
        index += consumed;
        let command = command.trim().to_owned();
        if is_ai_memory_hermes_hook(&command) {
            hooks.push((event.clone(), command));
        }
    }
    hooks
}

/// A YAML scalar starting at `first`, continued over `rest` when it folds onto
/// more lines than the key's (`indent`). Returns the value and how many lines
/// of `rest` it used.
fn scalar(first: &str, rest: &[&str], indent: usize) -> (String, usize) {
    let first = first.trim_start();
    let quote = first.chars().next().filter(|c| *c == '\'' || *c == '"');
    let Some(quote) = quote else {
        let mut value = strip_comment(first).trim_end().to_owned();
        let mut used = 0;
        for line in rest {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || line.len() - trimmed.len() <= indent {
                break;
            }
            value.push(' ');
            value.push_str(strip_comment(trimmed).trim_end());
            used += 1;
        }
        return (value, used);
    };
    let mut text = first[1..].to_owned();
    let mut used = 0;
    loop {
        if let Some(value) = close_quoted(&text, quote) {
            return (value, used);
        }
        let Some(line) = rest.get(used) else {
            // Unterminated: Hermes would not load this config at all.
            return (String::new(), used);
        };
        text.push(' ');
        text.push_str(line.trim());
        used += 1;
    }
}

/// The unescaped value of a quoted scalar whose opening quote is already
/// consumed, or `None` while its closing quote is still on a later line.
fn close_quoted(text: &str, quote: char) -> Option<String> {
    let mut value = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            ('\'', '\'') if chars.peek() == Some(&'\'') => {
                chars.next();
                value.push('\'');
            }
            ('"', '\\') => match chars.next()? {
                'n' => value.push('\n'),
                't' => value.push('\t'),
                other => value.push(other),
            },
            (_, c) if c == quote => return Some(value),
            (_, c) => value.push(c),
        }
    }
    None
}

/// A YAML comment starts at a `#` after whitespace, outside a quoted scalar
/// (callers only strip plain text).
fn strip_comment(text: &str) -> &str {
    text.find(" #").map_or(text, |at| &text[..at])
}

/// The commands `install-hooks --agent hermes` renders: the binary's `hook`
/// subcommand with `--agent hermes`, wherever the binary lives.
fn is_ai_memory_hermes_hook(command: &str) -> bool {
    let argv: Vec<&str> = command.split_whitespace().collect();
    argv.contains(&"hook") && argv.windows(2).any(|pair| pair == ["--agent", "hermes"])
}

/// Human lines for the Hermes homes; empty when Hermes is not installed.
pub(crate) fn render(reports: &[HermesProfileReport]) -> String {
    let mut out = String::new();
    for report in reports {
        let profile = crate::commands::show::terminal_text(&report.profile);
        let home = crate::commands::show::terminal_text(&report.home);
        let events = report
            .not_allowlisted
            .iter()
            .map(|event| crate::commands::show::terminal_text(event))
            .collect::<Vec<_>>();
        let flag = if report.profile == "default" {
            String::new()
        } else {
            format!(" -p {profile}")
        };
        let hooks = report.hook_events.len();
        let state = if hooks == 0 {
            "no ai-memory hooks in config.yaml".to_owned()
        } else if report.auto_accept {
            format!("{hooks} ai-memory hook event(s), approved by hooks_auto_accept")
        } else if report.not_allowlisted.is_empty() {
            format!("{hooks} ai-memory hook event(s), all allowlisted")
        } else {
            format!(
                "{hooks} ai-memory hook event(s), {} not allowlisted",
                report.not_allowlisted.len()
            )
        };
        out.push_str(&format!("  hermes ({}): {} ({state})\n", profile, home));
        if hooks > 0 && !report.auto_accept && !report.not_allowlisted.is_empty() {
            out.push_str(&format!(
                "⚠ Hermes profile {profile}: {events} not in {home}/{ALLOWLIST_FILENAME}, so \
                 Hermes skips those ai-memory hooks and captures nothing for them (the gateway \
                 logs \"not allowlisted — skipped\"). Approve them once with `hermes{flag} \
                 --accept-hooks chat`, or run `hermes{flag}` and accept the prompt; a running \
                 gateway picks the approval up. `hermes{flag} hooks doctor` shows each hook.\n",
                profile = profile,
                events = events.join(", "),
                home = home,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOOK: &str = "/opt/ai-memory --data-dir /data hook --event pre-tool-use --agent hermes \
                        --server-url http://127.0.0.1:49374";
    const END: &str = "/opt/ai-memory --data-dir /data hook --event stop --agent hermes \
                       --server-url http://127.0.0.1:49374";

    fn write_home(home: &Path, config: &str, approvals: &[(&str, &str)]) {
        std::fs::create_dir_all(home).unwrap();
        std::fs::write(home.join("config.yaml"), config).unwrap();
        let approvals: Vec<_> = approvals
            .iter()
            .map(|(event, command)| {
                serde_json::json!({
                    "event": event,
                    "command": command,
                    "approved_at": "2026-10-19T17:25:30Z",
                    "script_mtime_at_approval": null,
                })
            })
            .collect();
        std::fs::write(
            home.join(ALLOWLIST_FILENAME),
            serde_json::json!({ "approvals": approvals }).to_string(),
        )
        .unwrap();
    }

    fn config(hooks: &[(&str, &str)]) -> String {
        let mut out = String::from("model: x\nhooks:\n");
        for (event, command) in hooks {
            out.push_str(&format!(
                "  {event}:\n    - command: '{command}'\n      timeout: 20\n"
            ));
        }
        out.push_str("display:\n  skin: default\n");
        out
    }

    /// The block `install-hooks --agent hermes` prints reads back as the
    /// exact commands it wired, and nothing else does.
    #[test]
    fn reads_the_rendered_hermes_block() {
        let block = crate::commands::render_shared::build_hermes_hooks_yaml(
            "http://127.0.0.1:49374",
            None,
            Some(Path::new("/data dir/ai-memory")),
            None,
            crate::commands::render_shared::HermesHookSet::Full,
        );
        let hooks = ai_memory_hooks(&format!("model: x\n{block}terminal:\n  cwd: .\n"));
        let events: Vec<&str> = hooks.iter().map(|(event, _)| event.as_str()).collect();
        assert_eq!(
            events,
            [
                "pre_tool_call",
                "post_tool_call",
                "on_session_start",
                "pre_llm_call",
                "on_session_end",
                "on_session_finalize"
            ]
        );
        for (_, command) in &hooks {
            assert!(block.contains(&command.replace('\'', "''")), "{command}");
        }
    }

    /// Plain, double-quoted and folded scalars all read back as one command;
    /// another tool's hook, a commented-out one and a nested `hooks:` key are
    /// ignored.
    #[test]
    fn reads_every_scalar_style_and_ignores_other_hooks() {
        let yaml = format!(
            "hooks:\n  \
               pre_tool_call:\n    \
                 - command: {HOOK}\n    \
                 - command: \"/usr/bin/notify --agent hermes\"\n  \
               post_tool_call:\n    \
                 - matcher: terminal\n      \
                   command: \"/opt/ai-memory --data-dir \\\"/data\\\" hook --event post-tool-use\n        \
                     --agent hermes\"\n  \
               # on_session_end:\n  \
               #   - command: '{END}'\n  \
               on_session_end:\n    \
                 - command: '/opt/ai-memory hook\n        --event stop --agent hermes'  # wired\n\
             plugins:\n  \
               hooks:\n    \
                 pre_tool_call:\n      \
                   - command: '{HOOK}'\n"
        );
        let hooks = ai_memory_hooks(&yaml);
        assert_eq!(
            hooks,
            [
                ("pre_tool_call".to_owned(), HOOK.to_owned()),
                (
                    "post_tool_call".to_owned(),
                    "/opt/ai-memory --data-dir \"/data\" hook --event post-tool-use --agent hermes"
                        .to_owned()
                ),
                (
                    "on_session_end".to_owned(),
                    "/opt/ai-memory hook --event stop --agent hermes".to_owned()
                ),
            ]
        );
        assert!(ai_memory_hooks("hooks: {pre_tool_call: []}\n").is_empty());
    }

    #[test]
    fn auto_accept_follows_hermes_truthiness() {
        for yes in [
            "hooks_auto_accept: true",
            "hooks_auto_accept: True  # ok",
            "hooks_auto_accept: 'yes'",
            "hooks_auto_accept: \"1\"",
        ] {
            assert!(auto_accept(yes), "{yes}");
        }
        for no in [
            "hooks_auto_accept: false",
            "hooks_auto_accept: 1",
            "  hooks_auto_accept: true",
            "model: x",
        ] {
            assert!(!auto_accept(no), "{no}");
        }
    }

    #[test]
    fn root_walks_up_from_a_profile_home() {
        assert_eq!(
            hermes_root_from(PathBuf::from("/home/u/.hermes/profiles/bot")),
            Some(PathBuf::from("/home/u/.hermes"))
        );
        assert_eq!(
            hermes_root_from(PathBuf::from("/opt/data")),
            Some(PathBuf::from("/opt/data"))
        );
        let env =
            |pairs: &[(&str, &str)]| crate::config::RuntimeEnv::with_process_env_for_tests(pairs);
        assert_eq!(
            hermes_root(
                &env(&[("HERMES_HOME", "/opt/data")]),
                Some(Path::new("/home/u"))
            ),
            Some(PathBuf::from("/opt/data"))
        );
        assert_eq!(
            hermes_root(
                &env(&[("HERMES_HOME", "/home/u/.hermes/profiles/bot")]),
                None
            ),
            Some(PathBuf::from("/home/u/.hermes"))
        );
        // Blank is unset, and no home means no root.
        assert_eq!(hermes_root(&env(&[("HERMES_HOME", "")]), None), None);
    }

    /// The failure this check exists for: a profile wired in config.yaml with
    /// no approvals in its own allowlist is flagged, approvals in the root
    /// do not count for it, and an approval for a different command string
    /// (an upgrade moved the binary) does not either.
    #[test]
    fn flags_each_profile_against_its_own_allowlist() {
        let root = tempfile::tempdir().unwrap();
        let wired = config(&[("pre_tool_call", HOOK), ("on_session_end", END)]);
        write_home(
            root.path(),
            &wired,
            &[("pre_tool_call", HOOK), ("on_session_end", END)],
        );
        let bots = root.path().join("profiles");
        write_home(&bots.join("bot"), &wired, &[]);
        write_home(
            &bots.join("moved"),
            &wired,
            &[
                ("pre_tool_call", HOOK),
                (
                    "on_session_end",
                    "/old/ai-memory hook --event stop --agent hermes",
                ),
            ],
        );
        write_home(
            &bots.join("open"),
            &format!("hooks_auto_accept: true\n{wired}"),
            &[],
        );
        write_home(&bots.join("plain"), "model: x\n", &[]);
        std::fs::create_dir_all(bots.join(".deleted").join("old")).unwrap();
        std::fs::write(
            bots.join(".deleted").join("old").join("config.yaml"),
            &wired,
        )
        .unwrap();
        std::fs::create_dir_all(bots.join("empty")).unwrap();
        write_home(&bots.join("bad;profile"), &wired, &[]);

        let reports = reports(root.path());
        let summary: Vec<(&str, usize, Vec<&str>, bool)> = reports
            .iter()
            .map(|report| {
                (
                    report.profile.as_str(),
                    report.hook_events.len(),
                    report.not_allowlisted.iter().map(String::as_str).collect(),
                    report.auto_accept,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("default", 2, vec![], false),
                ("bot", 2, vec!["pre_tool_call", "on_session_end"], false),
                ("moved", 2, vec!["on_session_end"], false),
                ("open", 2, vec!["pre_tool_call", "on_session_end"], true),
                ("plain", 0, vec![], false),
            ]
        );

        let text = render(&reports);
        assert!(text.contains("hermes (default)"), "{text}");
        assert!(text.contains("all allowlisted"), "{text}");
        assert!(
            text.contains("⚠ Hermes profile bot: pre_tool_call, on_session_end not in"),
            "{text}"
        );
        assert!(
            text.contains("`hermes -p bot --accept-hooks chat`"),
            "{text}"
        );
        assert!(
            text.contains("⚠ Hermes profile moved: on_session_end"),
            "{text}"
        );
        assert!(!text.contains("⚠ Hermes profile open"), "{text}");
        assert!(text.contains("approved by hooks_auto_accept"), "{text}");
        assert!(text.contains("hermes (plain)"), "{text}");
        assert!(!text.contains("--server-url"), "commands stay out: {text}");
    }

    #[test]
    fn human_report_strips_terminal_controls_from_paths_and_events() {
        let report = HermesProfileReport {
            profile: "default".into(),
            home: "/tmp/unsafe\u{1b}[31m".into(),
            hook_events: vec!["event\u{1b}[31m".into()],
            not_allowlisted: vec!["event\u{1b}[31m".into()],
            auto_accept: false,
        };

        let text = render(&[report]);
        assert!(!text.contains('\u{1b}'), "{text:?}");
    }

    /// A missing or unreadable allowlist approves nothing, as in Hermes; no
    /// Hermes home means no report at all.
    #[test]
    fn missing_allowlist_approves_nothing() {
        let root = tempfile::tempdir().unwrap();
        assert!(reports(root.path()).is_empty());
        write_home(root.path(), &config(&[("pre_tool_call", HOOK)]), &[]);
        std::fs::write(root.path().join(ALLOWLIST_FILENAME), "{not json").unwrap();
        assert_eq!(reports(root.path())[0].not_allowlisted, ["pre_tool_call"]);
        std::fs::remove_file(root.path().join(ALLOWLIST_FILENAME)).unwrap();
        assert_eq!(reports(root.path())[0].not_allowlisted, ["pre_tool_call"]);
        assert_eq!(render(&[]), "");
    }
}
