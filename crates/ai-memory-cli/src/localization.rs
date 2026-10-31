//! Presentation-only localization of clap metadata and parser diagnostics.

use ai_memory_core::i18n::{Language, Localizer};
use std::ffi::{OsStr, OsString};

pub(crate) fn language_hint(args: &[OsString], environment: Option<&OsStr>) -> Language {
    let mut selected = environment
        .and_then(OsStr::to_str)
        .and_then(|value| value.parse().ok())
        .unwrap_or_default();
    let mut args = args.iter().skip(1).take_while(|value| *value != "--");
    while let Some(value) = args.next() {
        let value = value.to_string_lossy();
        let language = if value == "--language" {
            args.next().and_then(|value| value.to_str())
        } else {
            value.strip_prefix("--language=")
        };
        if let Some(language) = language.and_then(|value| value.parse().ok()) {
            selected = language;
        }
    }
    selected
}

/// Translate a message from a path that never loads configuration. Hooks run
/// before `Config::load`, so only `AI_MEMORY_LANGUAGE` reaches them.
pub(crate) fn process_diagnostic(text: &str) -> String {
    let language = language_hint(
        &[],
        crate::config::RuntimeEnv::from_process()
            .var_os("AI_MEMORY_LANGUAGE")
            .as_deref(),
    );
    if language == Language::English {
        return text.to_owned();
    }
    Localizer::new(language).diagnostic(text)
}

/// A failure already rendered for the terminal in the operator's language.
#[derive(Debug)]
struct RenderedFailure(String);

impl std::fmt::Display for RenderedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RenderedFailure {}

/// Translate an error chain where it leaves the process. English keeps the
/// error untouched, so the source chain and its exit output stay as they were.
pub(crate) fn localize_failure(error: anyhow::Error, language: Language) -> anyhow::Error {
    if language == Language::English {
        return error;
    }
    let i18n = Localizer::new(language);
    let mut causes = error
        .chain()
        .map(|cause| i18n.diagnostic(&cause.to_string()));
    let mut text = i18n.format(
        "cli-error-report",
        &[("message", causes.next().unwrap_or_default())],
    );
    let causes = causes.collect::<Vec<_>>();
    if !causes.is_empty() {
        text.push_str("\n\n");
        text.push_str(&i18n.text("cli-error-caused-by"));
        for (index, cause) in causes.iter().enumerate() {
            let indented = cause.replace('\n', "\n       ");
            text.push_str(&if causes.len() == 1 {
                format!("\n    {indented}")
            } else {
                format!("\n{index:>5}: {indented}")
            });
        }
    }
    anyhow::Error::new(RenderedFailure(text))
}

/// The text `main` prints for a failed run, matching Rust's own `Error: {:?}`
/// for anything that was not rendered above.
pub fn failure_report(error: &anyhow::Error) -> String {
    error
        .downcast_ref::<RenderedFailure>()
        .map_or_else(|| format!("Error: {error:?}"), ToString::to_string)
}

pub(crate) fn command(mut command: clap::Command, path: &str, i18n: &Localizer) -> clap::Command {
    if i18n.language() == Language::English {
        return command;
    }
    if command.get_name() == "help" {
        return command.about(i18n.text("cli-help-option-help"));
    }
    command.build();
    if command.get_about().is_some() {
        command = command.about(i18n.text(&format!("cli-help-{path}-about")));
    }
    if command.get_long_about().is_some() {
        command = command.long_about(i18n.text(&format!("cli-help-{path}-long-about")));
    }
    let ids = command
        .get_arguments()
        .map(|arg| arg.get_id().clone())
        .collect::<Vec<_>>();
    for id in ids {
        let arg = command
            .get_arguments()
            .find(|arg| arg.get_id() == &id)
            .cloned();
        let Some(arg) = arg else {
            continue;
        };
        let path = if arg.is_global_set() { "root" } else { path };
        let base = format!("cli-help-{path}-{}", id.as_str().replace('_', "-"));
        let help = if id == "help" {
            Some(i18n.text("cli-help-option-help"))
        } else if id == "version" {
            Some(i18n.text("cli-help-option-version"))
        } else {
            arg.get_help().map(|_| i18n.text(&format!("{base}-help")))
        };
        let long_help = if id == "help" || id == "version" {
            help.clone()
        } else {
            arg.get_long_help()
                .map(|_| i18n.text(&format!("{base}-long-help")))
        };
        command = command.mut_arg(id, |mut arg| {
            if let Some(help) = help {
                arg = arg.help(help);
            }
            if let Some(help) = long_help {
                arg = arg.long_help(help);
            }
            let heading = if arg.is_positional() {
                "cli-help-arguments"
            } else {
                "cli-help-options"
            };
            arg.help_heading(i18n.text(heading))
        });
    }
    command = command
        .subcommand_help_heading(i18n.text("cli-help-commands"))
        .help_template(i18n.text("cli-help-template"));
    command.mut_subcommands(|subcommand| {
        let subpath = format!("{path}-{}", subcommand.get_name());
        self::command(subcommand, &subpath, i18n)
    })
}

pub(crate) fn exit(error: clap::Error, i18n: &Localizer) -> ! {
    use clap::error::ErrorKind;
    if i18n.language() == Language::English
        || matches!(
            error.kind(),
            ErrorKind::DisplayHelp
                | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                | ErrorKind::DisplayVersion
        )
    {
        error.exit();
    }
    eprintln!("{}", parse_error(&error, i18n));
    std::process::exit(error.exit_code());
}

fn parse_error(error: &clap::Error, i18n: &Localizer) -> String {
    use clap::error::{ContextKind, ErrorKind};
    let id = match error.kind() {
        ErrorKind::UnknownArgument => "cli-parse-unknown-argument",
        ErrorKind::InvalidSubcommand => "cli-parse-unknown-command",
        ErrorKind::MissingSubcommand => "cli-parse-missing-command",
        ErrorKind::MissingRequiredArgument => "cli-parse-required-argument",
        ErrorKind::ArgumentConflict => "cli-parse-conflicting-arguments",
        ErrorKind::TooManyValues | ErrorKind::TooFewValues | ErrorKind::WrongNumberOfValues => {
            "cli-parse-value-count"
        }
        _ => "cli-parse-invalid-value",
    };
    let detail = [
        ContextKind::InvalidArg,
        ContextKind::InvalidSubcommand,
        ContextKind::InvalidValue,
    ]
    .into_iter()
    .filter_map(|kind| error.get(kind))
    .map(ToString::to_string)
    .collect::<Vec<_>>()
    .join("; ");
    format!(
        "{}: {}\n\n{}",
        i18n.text(id),
        detail,
        i18n.text("cli-parse-help-hint")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn help_catalogs_track_the_command_tree() {
        fn visit(command: clap::Command, path: &str, i18n: &Localizer) {
            for (suffix, value) in [
                ("about", command.get_about()),
                ("long-about", command.get_long_about()),
            ] {
                if let Some(value) = value {
                    assert_eq!(
                        i18n.text(&format!("cli-help-{path}-{suffix}")),
                        value.to_string()
                    );
                }
            }
            for arg in command.get_arguments() {
                for (suffix, value) in
                    [("help", arg.get_help()), ("long-help", arg.get_long_help())]
                {
                    if let Some(value) = value {
                        let id = format!(
                            "cli-help-{path}-{}-{suffix}",
                            arg.get_id().as_str().replace('_', "-")
                        );
                        assert_eq!(
                            i18n.text(&id),
                            value.to_string(),
                            "{id}: update both help catalogs when changing CLI metadata"
                        );
                    }
                }
            }
            for sub in command.get_subcommands() {
                visit(sub.clone(), &format!("{path}-{}", sub.get_name()), i18n);
            }
        }
        visit(crate::cli::Cli::command(), "root", &Localizer::default());
    }

    #[test]
    fn portuguese_help_contains_translated_descriptions_and_keeps_command_names() {
        let i18n = Localizer::new(Language::PortugueseBrazil);
        let mut command = command(crate::cli::Cli::command(), "root", &i18n);
        let help = command.render_long_help().to_string();
        assert!(help.contains("Comandos:"), "{help}");
        assert!(help.contains("Uso:"), "{help}");
        assert!(help.contains("--language"));
        assert!(help.contains("Exibir ajuda"));
        fn visit(command: &mut clap::Command) {
            let help = command.render_long_help().to_string();
            assert!(
                !help.contains("cli-help-"),
                "unresolved translation in {help}"
            );
            for sub in command.get_subcommands_mut() {
                visit(sub);
            }
        }
        visit(&mut command);
    }

    #[test]
    fn wrapper_language_does_not_consume_a_native_language_flag() {
        let cli = crate::cli::try_parse_from([
            "ai-memory",
            "--language",
            "pt-BR",
            "run",
            "codex",
            "--language",
            "en",
        ])
        .unwrap();
        assert_eq!(cli.language, Some(Language::PortugueseBrazil));
        let crate::cli::Command::Run(run) = cli.command else {
            panic!("run command");
        };
        assert_eq!(run.native_args, ["--language", "en"]);
    }

    #[test]
    fn failures_are_translated_for_the_terminal_and_keep_english_unchanged() {
        let failure = || anyhow::anyhow!("permission denied").context("reading /tmp/x");
        let english = localize_failure(failure(), Language::English);
        assert_eq!(failure_report(&english), format!("Error: {:?}", failure()));
        let report = failure_report(&localize_failure(failure(), Language::PortugueseBrazil));
        assert_eq!(
            report,
            "Erro: lendo /tmp/x\n\nCausado por:\n    permission denied"
        );
    }

    /// Catches a new English error or status line that never reached the
    /// catalog. Add it to both `diagnostics.ftl` files with the source wording.
    #[test]
    fn every_cli_diagnostic_has_a_catalog_entry() {
        fn unescape(raw: &str) -> String {
            let mut out = String::new();
            let mut chars = raw.chars().peekable();
            while let Some(ch) = chars.next() {
                if ch != '\\' {
                    out.push(ch);
                    continue;
                }
                match chars.next() {
                    Some('\n') => {
                        while chars.peek().is_some_and(|c| c.is_whitespace()) {
                            chars.next();
                        }
                    }
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('x') => {
                        chars.next();
                        chars.next();
                        out.push(' ');
                    }
                    Some(other) => out.push(other),
                    None => {}
                }
            }
            out
        }
        fn render_format(text: &str) -> String {
            let mut out = String::new();
            let mut chars = text.chars().peekable();
            while let Some(ch) = chars.next() {
                match (ch, chars.peek()) {
                    ('{', Some('{')) | ('}', Some('}')) => {
                        out.push(ch);
                        chars.next();
                    }
                    ('{', _) => {
                        chars.find(|c| *c == '}');
                        out.push('x');
                    }
                    _ => out.push(ch),
                }
            }
            out
        }
        fn visit(path: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(&path, files);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    files.push(path);
                }
            }
        }
        let call = regex::Regex::new(
            r#"(?:\bbail!|\banyhow!|\.context|\.with_context|\bdiagnostic)\s*\(\s*(?:\|\|\s*)?&?(?:format!\(\s*)?"((?:[^"\\]|\\[\s\S])*)""#,
        )
        .unwrap();
        let test_module =
            regex::Regex::new(r"(?m)^\s*#\[cfg\(test\)\]\s*\n\s*(?:pub(?:\(crate\))? )?mod \w+")
                .unwrap();
        // Literal arguments that name an operation, internal invariants, and
        // messages assembled from a variable part.
        let exempt = |text: &str| {
            text.starts_with("internal:")
                || text.starts_with("unterminated Grok")
                || text.starts_with("auto_improve.scheduler.experience_")
                || matches!(text, "reindex" | "reset" | "restore" | "purge data")
        };
        let i18n = Localizer::new(Language::PortugueseBrazil);
        let mut files = Vec::new();
        visit(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let mut missing = Vec::new();
        for file in files {
            // A Windows checkout has CRLF, which rustc treats as a plain newline.
            let source = std::fs::read_to_string(&file)
                .unwrap()
                .replace("\r\n", "\n");
            let body = test_module
                .find(&source)
                .map_or(source.as_str(), |found| &source[..found.start()]);
            for captures in call.captures_iter(body) {
                let text = unescape(&captures[1]);
                let rendered = render_format(&text);
                let has_words = rendered
                    .split(|c: char| !c.is_ascii_alphabetic())
                    .any(|word| word.len() > 1);
                if has_words && !exempt(&text) && !i18n.knows_diagnostic(&rendered) {
                    missing.push(format!("{}: {text}", file.display()));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "missing from diagnostics.ftl:\n{}",
            missing.join("\n")
        );
    }

    #[test]
    fn parser_errors_have_localized_context() {
        let error =
            crate::cli::try_parse_from(["ai-memory", "status", "--not-a-flag"]).unwrap_err();
        let message = parse_error(&error, &Localizer::new(Language::PortugueseBrazil));
        assert!(message.contains("Argumento desconhecido"), "{message}");
        assert!(message.contains("--not-a-flag"));
    }
}
