//! Explicit formatting for human diagnostic reports. Never intercepts output.

use std::fmt::Arguments;
use std::io::IsTerminal as _;

use crossterm::style::{Color, Stylize as _};

use crate::config::RuntimeEnv;

pub(crate) struct Terminal {
    pub(crate) i18n: ai_memory_core::i18n::Localizer,
    color: bool,
    columns: usize,
}

impl Terminal {
    pub(crate) fn stdout(env: &RuntimeEnv, i18n: &ai_memory_core::i18n::Localizer) -> Self {
        Self {
            i18n: i18n.clone(),
            columns: crossterm::terminal::size().map_or(80, |(columns, _)| usize::from(columns)),
            ..Self::new(std::io::stdout().is_terminal(), env)
        }
    }

    fn new(tty: bool, env: &RuntimeEnv) -> Self {
        Self {
            i18n: ai_memory_core::i18n::Localizer::default(),
            columns: 80,
            color: tty
                && env.var_os("NO_COLOR").is_none()
                && env.var_os("TERM").is_none_or(|term| term != "dumb"),
        }
    }

    pub(crate) fn header(&self, command: &str) {
        if self.color {
            println!(
                "\n  {}\n  {}\n  {}\n",
                "◈ ai-memory".cyan().bold(),
                command,
                "────────────────────────────────────────────────".dark_grey(),
            );
        }
    }

    pub(crate) fn section(&self, title: &str) {
        if self.color {
            println!("\n  {}", title.to_uppercase().cyan().bold());
        }
    }

    pub(crate) fn line(&self, args: Arguments<'_>) {
        println!("{}", self.paint(&args.to_string()));
    }

    pub(crate) fn text(&self, text: &str) {
        print!("{}", self.paint(text));
    }

    /// Plain reports remain exact; interactive reports add margins and wrap prose.
    fn paint(&self, text: &str) -> String {
        if !self.color {
            return text.to_owned();
        }
        text.split_inclusive('\n')
            .map(|line| {
                let value = line.trim_end_matches('\n');
                let trimmed = value.trim_start();
                let value = self.layout(value);
                let color = if trimmed.starts_with(['⚠', '!', '?']) {
                    Some(Color::Yellow)
                } else if trimmed.starts_with('✓') {
                    Some(Color::Green)
                } else if ["pages:", "sessions:", "observations:"]
                    .iter()
                    .any(|key| trimmed.starts_with(key))
                {
                    Some(Color::Cyan)
                } else {
                    None
                };
                let decorated = if let Some(color) = color {
                    value.with(color).bold().to_string()
                } else if value.starts_with(' ')
                    && let Some((label, rest)) = value.split_once(':')
                    && label.trim().len() <= 22
                {
                    format!("{}:{rest}", label.dark_grey())
                } else {
                    value
                };
                format!(
                    "{decorated}{}",
                    if line.ends_with('\n') { "\n" } else { "" }
                )
            })
            .collect()
    }

    fn layout(&self, line: &str) -> String {
        if line.is_empty() {
            return String::new();
        }
        let text = line.trim_start();
        let indent = " ".repeat((line.len() - text.len()).max(2));
        let mut result = String::new();
        let mut rest = text;
        // Byte lengths are conservative for Unicode terminal columns. Keep long
        // tokens intact, and only wrap at spaces: paths and words are never cut.
        let available = self.columns.saturating_sub(indent.len() + 1).max(1);
        while rest.len() > available {
            let split = rest
                .char_indices()
                .take_while(|(index, _)| *index <= available)
                .filter(|(_, ch)| *ch == ' ')
                .map(|(index, _)| index)
                .last();
            let Some(split) = split.filter(|index| *index > 0) else {
                break;
            };
            result.push_str(&indent);
            result.push_str(&rest[..split]);
            result.push('\n');
            rest = &rest[split + 1..];
        }
        result.push_str(&indent);
        result.push_str(rest);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_no_color_and_dumb_terminals_keep_exact_report_text() {
        let text = "  pages: 12\n⚠ Review hooks\n    ai-memory install-hooks --agent codex --apply\n  db: C:\\memory dir\\db.sqlite\n";
        for (tty, pairs) in [
            (false, vec![]),
            (true, vec![("NO_COLOR", "")]),
            (true, vec![("NO_COLOR", "1")]),
            (true, vec![("TERM", "dumb")]),
        ] {
            let terminal = Terminal::new(tty, &RuntimeEnv::with_process_env_for_tests(&pairs));
            assert!(!terminal.color);
            assert_eq!(terminal.paint(text), text);
        }
    }

    #[test]
    fn interactive_margins_and_wrapping_keep_words_and_nested_indentation() {
        let terminal = Terminal {
            i18n: ai_memory_core::i18n::Localizer::default(),
            color: true,
            columns: 48,
        };
        for text in [
            "ai-memory 3.0.0 (server)",
            "Capture coverage for preview/onboarding (server http://127.0.0.1:49374)",
            "⚠ Codex skips its untrusted hooks. Open Codex, run /hooks, and trust them.",
            "      └ nested remedy",
            "        ai-memory install-hooks --agent claude-code --apply",
        ] {
            let rendered = terminal.layout(text);
            assert_eq!(
                rendered.split_whitespace().collect::<Vec<_>>(),
                text.split_whitespace().collect::<Vec<_>>()
            );
            assert!(
                rendered
                    .lines()
                    .all(|line| line.starts_with("  ") && line.len() <= 48)
            );
            if text.starts_with("      ") {
                assert!(rendered.lines().all(|line| line.starts_with("      ")));
            }
        }
        assert_eq!(terminal.layout(""), "");
        assert_eq!(terminal.layout("  "), "  ");
    }

    #[test]
    fn styled_reports_preserve_words_including_unknown_fields() {
        let terminal = Terminal::new(
            true,
            &RuntimeEnv::with_process_env_for_tests(&[("TERM", "xterm-256color")]),
        );
        let text = "  pages: 42\n  future field: /a/path with spaces\n✓ Agent captured\n⚠ Unknown warning; keep this remedy\n? Capture unknown\n";
        let decorated = terminal.paint(text);
        assert!(decorated.contains('\u{1b}'));
        let mut plain = String::new();
        let mut chars = decorated.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                for code in chars.by_ref() {
                    if code == 'm' {
                        break;
                    }
                }
            } else {
                plain.push(ch);
            }
        }
        assert_eq!(
            plain.split_whitespace().collect::<Vec<_>>(),
            text.split_whitespace().collect::<Vec<_>>()
        );
        assert!(plain.lines().all(|line| line.starts_with("  ")));
    }
}
