//! `ai-memory completions <shell>` — print a shell-completion script.
//!
//! Operator workflow (fish shown; see `docs/shell-completions.md` for the
//! other shells):
//!   $ ai-memory completions fish > ~/.config/fish/completions/ai-memory.fish
//!
//! The script is generated from the same derived `clap::Command` the parser
//! builds, so every subcommand, flag, and help string stays in lockstep with
//! the binary that produced it. Nothing is vendored into the repo: a stale
//! checked-in script is worse than no script at all.

use std::io::Write;

use anyhow::{Context, Result};
use clap::CommandFactory;
use clap_complete::generate;

use crate::cli::{Cli, CompletionsArgs};

/// Run the `completions` subcommand.
///
/// Writes to stdout so the caller can redirect into their shell's completion
/// directory. Deliberately takes no `&Config`: generating a script must work
/// before `ai-memory init`, on a machine with no data directory, and inside a
/// container build step.
///
/// # Errors
/// Propagates I/O failures from writing to stdout.
pub fn run(args: CompletionsArgs) -> Result<()> {
    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_string();

    // Render into a buffer rather than straight at stdout: clap_complete
    // `.expect()`s on write errors, so a closed pipe (`... | head`) would
    // surface as a panic instead of the silent exit every other CLI gives.
    let mut script = Vec::new();
    generate(args.shell, &mut cmd, bin_name, &mut script);

    let stdout = std::io::stdout();
    write_script(stdout.lock(), &script)
}

fn write_script(mut output: impl Write, script: &[u8]) -> Result<()> {
    match output.write_all(script) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.context("writing completion script to stdout"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "reader closed",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn closed_output_pipe_is_a_clean_exit() {
        write_script(ClosedPipe, b"completion script").unwrap();
    }
}
