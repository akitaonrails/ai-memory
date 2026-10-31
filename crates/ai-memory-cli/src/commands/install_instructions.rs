//! `ai-memory install-instructions` — drop the proactive-use snippet
//! into a project's `CLAUDE.md` / `AGENTS.md` / other rules file.
//!
//! ## Why this exists
//!
//! Lifecycle hooks handle *capture* and *handoff surfacing*
//! automatically. What they can't do is make the agent *proactively
//! call* `memory_query` / `memory_recent` when it should — that
//! decision lives in the model's system prompt, fed turn-by-turn by
//! the project's CLAUDE.md / AGENTS.md.
//!
//! This subcommand drops a small, opinionated snippet into that
//! file. Idempotent via HTML-comment markers so re-running picks up
//! whatever the snippet evolves into without duplicating the block.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::{
    InstallInstructionsArgs, InstallSkillsAgent, InstallSkillsArgs, InstallSkillsScope,
};
use crate::commands::apply_shared::{ApplyOutcome, apply_atomic};
use crate::commands::install_skills;
use crate::config::Config;

// Markers + the snippet body live in `ai_memory_core::routing_snippet`
// so the `memory_install_self_routing` MCP tool can return the same
// block this subcommand writes. Single source of truth.
use ai_memory_core::{MARKER_END, MARKER_START, compact_block, find_marker_line, full_block};

const LEGACY_ORPHAN_TAIL_LF: &str =
    "` markers without\ndisturbing the rest of the file.\n<!-- ai-memory:end -->\n";
const LEGACY_ORPHAN_TAIL_CRLF: &str =
    "` markers without\r\ndisturbing the rest of the file.\r\n<!-- ai-memory:end -->\r\n";

/// Run the `install-instructions` subcommand.
///
/// # Errors
/// Returns an error if the target path can't be written or if the
/// existing file isn't valid UTF-8.
pub fn run(config: &Config, args: InstallInstructionsArgs) -> Result<()> {
    let i18n = &config.i18n;
    run_inner(i18n, args, &config.runtime_env, true)
}

pub(super) fn run_quiet(
    i18n: &ai_memory_core::i18n::Localizer,
    args: InstallInstructionsArgs,
    env: &crate::config::RuntimeEnv,
) -> Result<()> {
    run_inner(i18n, args, env, false)
}

fn run_inner(
    i18n: &ai_memory_core::i18n::Localizer,
    args: InstallInstructionsArgs,
    env: &crate::config::RuntimeEnv,
    report: bool,
) -> Result<()> {
    let block = if args.compact {
        compact_block()
    } else {
        full_block()
    };
    let targets = resolve_targets(i18n, args.target.as_ref())?;
    if report {
        let home = env
            .var_os("HOME")
            .or_else(|| env.var_os("USERPROFILE"))
            .map(PathBuf::from);
        if let Ok(cwd) = std::env::current_dir()
            && let Some(note) = claude_code_skips_agents_md(&targets, &cwd, home.as_deref())
        {
            eprintln!("{note}");
        }
    }
    let skill_args = if args.no_skills {
        None
    } else {
        Some(skill_args_from_instruction_args(&args, &targets))
    };
    let prepared_skills = if !args.print {
        skill_args
            .as_ref()
            .map(|args| install_skills::prepare_install(args, env))
            .transpose()?
    } else {
        None
    };

    if args.print {
        for t in &targets {
            println!(
                "{}",
                i18n.format(
                    "cli-install-instructions-would-write-into",
                    &[("value0", format!("{}", t.display()))]
                )
            );
            println!("{block}");
        }
    } else {
        for target in &targets {
            let outcome = apply_atomic(target, |existing| {
                Ok(merge_instructions_block(existing, &block))
            })?;
            if report {
                println!(
                    "✓ {} {} ({})",
                    outcome.verb(),
                    target.display(),
                    match outcome {
                        ApplyOutcome::Created => "new file",
                        ApplyOutcome::Updated => "backup written next to it",
                        ApplyOutcome::NoOp => "already up to date",
                    }
                );
            }
        }
    }

    if let Some(prepared_skills) = prepared_skills {
        if report {
            install_skills::run_prepared(prepared_skills)?;
        } else {
            install_skills::run_prepared_quiet(prepared_skills)?;
        }
    }

    Ok(())
}

/// Decide which file(s) the snippet should land in.
///
/// Precedence:
/// 1. `--target` passed explicitly → use exactly that path (one file).
/// 2. Both `CLAUDE.md` and `AGENTS.md` exist in `$PWD` → write to both
///    (a project that's set up for multiple agent CLIs deserves the
///    snippet in each convention).
/// 3. Only `CLAUDE.md` exists → write to it.
/// 4. Only `AGENTS.md` exists → write to it.
/// 5. Neither exists → default to `CLAUDE.md` AND print a hint about
///    `--target AGENTS.md` for Codex / OpenCode / Cursor / Gemini /
///    Kimi Code.
///
/// The auto-pick exists because Claude Code uses CLAUDE.md while
/// every other supported agent (Codex, OpenCode, Cursor, Gemini CLI,
/// Kimi Code) converged on AGENTS.md. The heuristic "extend whatever's
/// already there" matches the user's intent better than a hard-coded
/// default.
fn resolve_targets(
    i18n: &ai_memory_core::i18n::Localizer,
    explicit: Option<&std::path::PathBuf>,
) -> Result<Vec<std::path::PathBuf>> {
    if let Some(p) = explicit {
        return Ok(vec![p.clone()]);
    }
    let cwd = std::env::current_dir().context(
        i18n.text("cli-install-instructions-getting-cwd-for-install-instructions-target"),
    )?;
    let claude_md = cwd.join("CLAUDE.md");
    let agents_md = cwd.join("AGENTS.md");
    let has_claude = claude_md.exists();
    let has_agents = agents_md.exists();
    match (has_claude, has_agents) {
        (true, true) => Ok(vec![claude_md, agents_md]),
        (true, false) => Ok(vec![claude_md]),
        (false, true) => Ok(vec![agents_md]),
        (false, false) => {
            eprintln!(
                "{}", i18n.format("cli-install-instructions-note-neither-claude-md-nor-agents-md-exists-in-creating-claude-md-if-you-use", &[("value0", format!("{}", cwd.display()))]));
            Ok(vec![claude_md])
        }
    }
}

/// A note for when the block goes only into `AGENTS.md` but Claude Code will
/// not read that file (#1297, #1298). Claude Code 2.1.277 and later reads
/// `AGENTS.md` only when no `CLAUDE.md`, `.claude/CLAUDE.md` or
/// `CLAUDE.local.md` exists in the working directory or above it; when one
/// does, it reads those instead. The block still reaches Claude Code when one
/// of those files imports the `AGENTS.md` being written. `@path` resolves from
/// the importing file (`~/` from the home directory), including mid-sentence;
/// code spans and fenced blocks are not imports. The user file
/// `~/.claude/CLAUDE.md` does not count.
fn claude_code_skips_agents_md(
    targets: &[PathBuf],
    cwd: &Path,
    home: Option<&Path>,
) -> Option<String> {
    let named = |name: &str| {
        targets
            .iter()
            .any(|target| target.file_name().and_then(|n| n.to_str()) == Some(name))
    };
    if !named("AGENTS.md") || named("CLAUDE.md") {
        return None;
    }
    let agents = written_agents_files(targets, cwd);
    let first_agents = agents.first()?;
    let mut found = Vec::new();
    for dir in cwd.ancestors() {
        for name in ["CLAUDE.md", "CLAUDE.local.md", ".claude/CLAUDE.md"] {
            if name == ".claude/CLAUDE.md" && home.is_some_and(|h| paths_match(h, dir)) {
                continue;
            }
            let candidate = dir.join(name);
            if candidate.is_file() {
                found.push(candidate);
            }
        }
    }
    let first = found.first()?;
    if found
        .iter()
        .any(|path| file_imports_written_agents(path, &agents, home))
    {
        return None;
    }
    let import = suggested_agents_import(first, first_agents);
    Some(format!(
        "note: {} exists, so Claude Code reads it instead of AGENTS.md and will not see \
         this block. Add `{import}` to that file, or set Claude \
         Code's Project instructions to `claude-md-and-agents-md`.",
        first.display(),
    ))
}

fn written_agents_files(targets: &[PathBuf], cwd: &Path) -> Vec<PathBuf> {
    targets
        .iter()
        .filter(|target| target.file_name().and_then(|name| name.to_str()) == Some("AGENTS.md"))
        .map(|target| {
            if target.is_absolute() {
                target.clone()
            } else {
                cwd.join(target)
            }
        })
        .collect()
}

/// How many import hops Claude Code follows from a memory file: an imported
/// file may import others, up to this depth.
const MAX_IMPORT_HOPS: usize = 4;
/// One imported instruction file can be user-controlled. Detection only needs
/// enough text to find ordinary `@path` directives, so do not allocate an
/// arbitrarily large file merely to decide whether Claude reads AGENTS.md.
const MAX_IMPORT_FILE_BYTES: usize = 256 * 1024;
/// Bound a broad import graph as well as each individual file.
const MAX_IMPORT_TOTAL_BYTES: usize = 1024 * 1024;

fn file_imports_written_agents(path: &Path, agents: &[PathBuf], home: Option<&Path>) -> bool {
    let mut remaining = MAX_IMPORT_TOTAL_BYTES;
    imports_within(
        path,
        agents,
        home,
        MAX_IMPORT_HOPS,
        &mut std::collections::HashSet::new(),
        &mut remaining,
    )
}

/// Whether `path`, or a file it imports within `hops` hops, imports one of
/// `agents`. A file already visited is not read again, so an import cycle
/// ends.
fn imports_within(
    path: &Path,
    agents: &[PathBuf],
    home: Option<&Path>,
    hops: usize,
    seen: &mut std::collections::HashSet<PathBuf>,
    remaining: &mut usize,
) -> bool {
    if hops == 0 || !seen.insert(path.to_path_buf()) {
        return false;
    }
    let Some(text) = read_bounded_import(path, remaining) else {
        return false;
    };
    claude_import_paths(&text).into_iter().any(|raw| {
        let resolved = resolve_claude_import(&raw, path, home);
        agents.iter().any(|target| paths_match(&resolved, target))
            || (resolved.is_file()
                && imports_within(&resolved, agents, home, hops - 1, seen, remaining))
    })
}

fn read_bounded_import(path: &Path, remaining: &mut usize) -> Option<String> {
    let allowance = (*remaining).min(MAX_IMPORT_FILE_BYTES);
    if allowance == 0 {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(allowance.min(8 * 1024));
    file.take((allowance + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    let consumed = bytes.len().min(allowance);
    *remaining = remaining.saturating_sub(consumed);
    if bytes.len() > allowance {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// `@path` relative to the importing file, so `.claude/CLAUDE.md` needs
/// `@../AGENTS.md` and a parent file needs `@app/AGENTS.md`.
fn suggested_agents_import(importer: &Path, agents: &Path) -> String {
    let from = importer.parent().unwrap_or(importer);
    format!("@{}", slash_relative(from, agents))
}

fn resolve_claude_import(raw: &str, importer: &Path, home: Option<&Path>) -> PathBuf {
    let base = importer.parent().unwrap_or(importer);
    if let Some(rest) = home_relative(raw)
        && let Some(home) = home
    {
        return if rest.is_empty() {
            home.to_path_buf()
        } else {
            home.join(rest)
        };
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn home_relative(raw: &str) -> Option<&str> {
    if raw == "~" {
        Some("")
    } else {
        raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\"))
    }
}

fn paths_match(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        // One side may not exist yet (`.claude/AGENTS.md` when only the
        // project file does). Fold `.` and `..` without requiring either file.
        _ => lexical_normalize(left) == lexical_normalize(right),
    }
}

/// Fold `.` and `..` without touching the filesystem. A `..` that would climb
/// past the root stays at the root; import paths are absolute by the time
/// they reach here, so this does not consult the process cwd.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut stack: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match stack.last() {
                Some(Component::Normal(_)) => {
                    stack.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => stack.push(component),
            },
            other => stack.push(other),
        }
    }
    stack.into_iter().collect()
}

fn slash_relative(from_dir: &Path, target: &Path) -> String {
    let from = lexical_normalize(from_dir);
    let to = lexical_normalize(target);
    let from_comps: Vec<Component<'_>> = from.components().collect();
    let to_comps: Vec<Component<'_>> = to.components().collect();
    let mut shared = 0;
    while shared < from_comps.len()
        && shared < to_comps.len()
        && from_comps[shared] == to_comps[shared]
    {
        shared += 1;
    }
    let root_left =
        |component: &Component<'_>| matches!(component, Component::Prefix(_) | Component::RootDir);
    if from_comps.iter().skip(shared).any(root_left) || to_comps.iter().skip(shared).any(root_left)
    {
        return forward_slashes(&to);
    }
    let mut parts = Vec::new();
    for _ in from_comps.iter().skip(shared) {
        parts.push("..".to_string());
    }
    for component in to_comps.iter().skip(shared) {
        if let Component::Normal(part) = component {
            parts.push(part.to_string_lossy().into_owned());
        }
    }
    if parts.is_empty() {
        forward_slashes(&to)
    } else {
        parts.join("/")
    }
}

fn forward_slashes(path: &Path) -> String {
    let mut out = String::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push_str(&prefix.as_os_str().to_string_lossy()),
            Component::RootDir => {
                if !out.ends_with('/') {
                    out.push('/');
                }
            }
            Component::CurDir => push_slash_part(&mut out, "."),
            Component::ParentDir => push_slash_part(&mut out, ".."),
            Component::Normal(part) => push_slash_part(&mut out, &part.to_string_lossy()),
        }
    }
    out
}

fn push_slash_part(out: &mut String, part: &str) {
    if !out.is_empty() && !out.ends_with(['/', ':']) {
        out.push('/');
    }
    out.push_str(part);
}

/// `@path` tokens Claude Code would import. Relative paths are not resolved
/// here; the caller joins them onto the importing file. Fenced blocks and
/// code spans are skipped, and a path ends at the first unescaped space. A
/// backtick run with no closer is literal text, matching CommonMark.
fn claude_import_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut prose = String::new();
    let mut fence: Option<(u8, usize)> = None;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        if let Some((tick, len)) = fence {
            if fence_close(body, tick, len) {
                fence = None;
            }
            continue;
        }
        if let Some(opened) = fence_open(body) {
            scan_import_prose(&prose, &mut paths);
            prose.clear();
            fence = Some(opened);
            continue;
        }
        prose.push_str(line);
    }
    scan_import_prose(&prose, &mut paths);
    paths
}

fn fence_open(line: &str) -> Option<(u8, usize)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && i < 3 && bytes[i] == b' ' {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let tick = bytes[i];
    if tick != b'`' && tick != b'~' {
        return None;
    }
    let start = i;
    while i < bytes.len() && bytes[i] == tick {
        i += 1;
    }
    let len = i - start;
    if len < 3 || (tick == b'`' && line[i..].contains('`')) {
        return None;
    }
    Some((tick, len))
}

fn fence_close(line: &str, tick: u8, min_len: usize) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && i < 3 && bytes[i] == b' ' {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i] == tick {
        i += 1;
    }
    i - start >= min_len && line[i..].trim().is_empty()
}

fn scan_import_prose(text: &str, paths: &mut Vec<String>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' && (i == 0 || bytes[i - 1] != b'`') {
            let len = byte_run_len(bytes, i, b'`');
            if let Some(end) = find_exact_run(bytes, i + len, b'`', len) {
                i = end;
                continue;
            }
            i += len;
            continue;
        }
        if bytes[i] == b'@' && import_boundary(bytes, i) {
            let (path, next) = read_import_path(text, i + 1);
            if import_path_ok(&path) {
                paths.push(path);
            }
            i = next.max(i + 1);
            continue;
        }
        i += 1;
    }
}

fn import_boundary(bytes: &[u8], at: usize) -> bool {
    if at == 0 {
        return true;
    }
    let prev = bytes[at - 1];
    if prev == b'"' || prev == b'\'' {
        return false;
    }
    !prev.is_ascii_alphanumeric()
        && !matches!(prev, b'_' | b'-' | b'.' | b'/' | b'\\' | b'@' | b'~')
}

fn import_path_ok(path: &str) -> bool {
    !path.is_empty() && !path.starts_with(['"', '\''])
}

fn read_import_path(line: &str, start: usize) -> (String, usize) {
    let bytes = line.as_bytes();
    let mut i = start;
    let mut out = String::new();
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && let Some(next) = bytes.get(i + 1).copied()
            && (next == b' ' || next == b'\\')
        {
            out.push(next as char);
            i += 2;
            continue;
        }
        if bytes[i].is_ascii_whitespace() {
            break;
        }
        let Some(ch) = line[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, i)
}

fn byte_run_len(bytes: &[u8], start: usize, tick: u8) -> usize {
    let mut i = start;
    while i < bytes.len() && bytes[i] == tick {
        i += 1;
    }
    i - start
}

/// Index just past a run of exactly `len` `tick` bytes that is not part of a
/// longer run. A longer run must not close a shorter span.
fn find_exact_run(bytes: &[u8], mut from: usize, tick: u8, len: usize) -> Option<usize> {
    while from < bytes.len() {
        let run_start = bytes[from] == tick && (from == 0 || bytes[from - 1] != tick);
        if run_start && byte_run_len(bytes, from, tick) == len {
            return Some(from + len);
        }
        from += 1;
    }
    None
}

fn skill_args_from_instruction_args(
    args: &InstallInstructionsArgs,
    targets: &[PathBuf],
) -> InstallSkillsArgs {
    InstallSkillsArgs {
        scope: args.skills_scope.unwrap_or(InstallSkillsScope::Project),
        agent: args
            .skills_agent
            .unwrap_or_else(|| infer_skills_agent_from_instruction_targets(targets)),
        target_dir: args.skills_target_dir.clone(),
        print: args.print,
        force: args.skills_force,
    }
}

fn infer_skills_agent_from_instruction_targets(targets: &[PathBuf]) -> InstallSkillsAgent {
    let mut has_claude_target = false;
    let mut has_agents_target = false;

    for target in targets {
        match target.file_name().and_then(|name| name.to_str()) {
            Some("CLAUDE.md") => has_claude_target = true,
            Some("AGENTS.md") => has_agents_target = true,
            _ => {}
        }
    }

    match (has_claude_target, has_agents_target) {
        (true, true) => InstallSkillsAgent::Both,
        (false, true) => InstallSkillsAgent::Agents,
        _ => InstallSkillsAgent::ClaudeCode,
    }
}

/// Idempotent merge: when the markers exist, replace everything
/// between them (inclusive) with `block`. When they don't, append
/// `block` to the end of the file with a single blank-line
/// separator. The user's other content is never touched.
fn merge_instructions_block(existing: &str, block: &str) -> String {
    // Anchor on markers that occupy their own line so an inline mention of
    // the marker strings (e.g. this block's own prose describing them)
    // cannot be mistaken for the real end delimiter — which would truncate
    // the block and leave an orphan tail on every refresh.
    if let Some(start_idx) = find_marker_line(existing, MARKER_START, 0)
        && let Some(end_pos) = find_marker_line(existing, MARKER_END, start_idx)
    {
        let end_idx = end_pos + MARKER_END.len();
        // Consume a trailing newline after the end marker if present
        // so we don't accumulate blank lines on every re-run.
        let after_end = if existing.as_bytes().get(end_idx..end_idx + 2) == Some(b"\r\n") {
            end_idx + 2
        } else if existing.as_bytes().get(end_idx).copied() == Some(b'\n') {
            end_idx + 1
        } else {
            end_idx
        };
        let tail = strip_legacy_orphan_tail(block, &existing[after_end..]);
        let mut out = String::with_capacity(existing.len() + block.len());
        out.push_str(&existing[..start_idx]);
        out.push_str(block);
        out.push_str(tail);
        return out;
    }
    // No prior block — append. If the file already ends with a
    // newline, separate with one blank line; otherwise add the
    // newline + a blank line.
    let mut out = existing.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(block);
    out
}

fn strip_legacy_orphan_tail<'a>(block: &str, tail: &'a str) -> &'a str {
    let dynamic_orphan = legacy_orphan_tail(block);
    let mut rest = tail;
    loop {
        if let Some(stripped) = rest.strip_prefix(LEGACY_ORPHAN_TAIL_LF) {
            rest = stripped;
        } else if let Some(stripped) = rest.strip_prefix(LEGACY_ORPHAN_TAIL_CRLF) {
            rest = stripped;
        } else if let Some(orphan) = dynamic_orphan
            && !orphan.is_empty()
            && let Some(stripped) = rest.strip_prefix(orphan)
        {
            rest = stripped;
        } else {
            return rest;
        }
    }
}

fn legacy_orphan_tail(block: &str) -> Option<&str> {
    let real_start = find_marker_line(block, MARKER_START, 0)?;
    let real_end = find_marker_line(block, MARKER_END, real_start + MARKER_START.len())?;
    let mut search_from = real_start + MARKER_START.len();
    while let Some(rel) = block[search_from..real_end].find(MARKER_END) {
        let inline = search_from + rel;
        if find_marker_line(block, MARKER_END, inline) != Some(inline) {
            let orphan_start = inline + MARKER_END.len();
            let orphan_end = consume_line_ending(block, real_end + MARKER_END.len());
            return Some(&block[orphan_start..orphan_end]);
        }
        search_from = inline + MARKER_END.len();
    }
    None
}

fn consume_line_ending(s: &str, idx: usize) -> usize {
    if s.as_bytes().get(idx..idx + 2) == Some(b"\r\n") {
        idx + 2
    } else if s.as_bytes().get(idx).copied() == Some(b'\n') {
        idx + 1
    } else {
        idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1298, measured against Claude Code 2.1.300. `@path` resolves from the
    /// importing file, including mid-sentence, and a fenced `@AGENTS.md` is
    /// not an import. Two of these are silent misses on the exact-line check
    /// and two are false notes; the parent-directory row is the third miss.
    /// Claude Code follows imports through other imported files, up to four
    /// hops, so an `AGENTS.md` reached through an intermediate file needs no
    /// note; one five hops away, or only reachable through a cycle, does.
    #[test]
    fn agents_md_reached_through_nested_imports_counts_up_to_four_hops() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "# rules\n").unwrap();
        let agents = [dir.join("AGENTS.md")];

        std::fs::write(dir.join("CLAUDE.md"), "@docs/a.md\n").unwrap();
        std::fs::write(dir.join("docs/a.md"), "@../AGENTS.md\n").unwrap();
        assert!(file_imports_written_agents(
            &dir.join("CLAUDE.md"),
            &agents,
            None
        ));

        // CLAUDE.md -> h1 -> h2 -> h3 -> h4 -> AGENTS.md is five hops.
        for (from, to) in [
            ("CLAUDE.md", "@docs/h1.md"),
            ("docs/h1.md", "@h2.md"),
            ("docs/h2.md", "@h3.md"),
            ("docs/h3.md", "@h4.md"),
            ("docs/h4.md", "@../AGENTS.md"),
        ] {
            std::fs::write(dir.join(from), format!("{to}\n")).unwrap();
        }
        assert!(!file_imports_written_agents(
            &dir.join("CLAUDE.md"),
            &agents,
            None
        ));

        // A cycle that never reaches AGENTS.md ends.
        std::fs::write(dir.join("CLAUDE.md"), "@docs/c1.md\n").unwrap();
        std::fs::write(dir.join("docs/c1.md"), "@c2.md\n").unwrap();
        std::fs::write(dir.join("docs/c2.md"), "@c1.md\n").unwrap();
        assert!(!file_imports_written_agents(
            &dir.join("CLAUDE.md"),
            &agents,
            None
        ));
    }

    #[test]
    fn claude_import_detection_refuses_oversized_and_non_utf8_files() {
        let root = tempfile::tempdir().unwrap();
        let agents = root.path().join("AGENTS.md");
        std::fs::write(&agents, "# rules\n").unwrap();

        let mut oversized = b"@AGENTS.md\n".to_vec();
        oversized.resize(MAX_IMPORT_FILE_BYTES + 1, b'x');
        let claude = root.path().join("CLAUDE.md");
        std::fs::write(&claude, oversized).unwrap();
        assert!(!file_imports_written_agents(
            &claude,
            std::slice::from_ref(&agents),
            None
        ));

        std::fs::write(&claude, [b'@', 0xff, b'\n']).unwrap();
        assert!(!file_imports_written_agents(
            &claude,
            std::slice::from_ref(&agents),
            None
        ));
    }

    #[test]
    fn claude_import_detection_bounds_the_whole_import_graph() {
        let root = tempfile::tempdir().unwrap();
        let agents = root.path().join("AGENTS.md");
        std::fs::write(&agents, "# rules\n").unwrap();
        std::fs::write(
            root.path().join("CLAUDE.md"),
            "@one.md\n@two.md\n@three.md\n@four.md\n@leaf.md\n",
        )
        .unwrap();
        for name in ["one.md", "two.md", "three.md", "four.md"] {
            std::fs::write(root.path().join(name), vec![b'x'; MAX_IMPORT_FILE_BYTES]).unwrap();
        }
        std::fs::write(root.path().join("leaf.md"), "@AGENTS.md\n").unwrap();

        assert!(!file_imports_written_agents(
            &root.path().join("CLAUDE.md"),
            std::slice::from_ref(&agents),
            None
        ));
    }

    #[test]
    fn claude_code_agents_import_matches_five_measured_cases() {
        struct Case<'a> {
            label: &'a str,
            /// Working directory, relative to the temp root. `.` is the root.
            cwd: &'a str,
            agents: &'a str,
            files: &'a [(&'a str, &'a str)],
            expect_note: bool,
            /// Import the note should tell the user to add, when it warns.
            import_hint: Option<&'a str>,
        }
        const FENCED: &str = "```\n@AGENTS.md\n```\n";
        let beside = [(".claude/CLAUDE.md", "@AGENTS.md\n")];
        let parent_import = [(".claude/CLAUDE.md", "@../AGENTS.md\n")];
        let mid_sentence = [("CLAUDE.local.md", "See @AGENTS.md for the team rules.\n")];
        let fenced = [("CLAUDE.local.md", FENCED)];
        let parent_dir = [("CLAUDE.local.md", "@AGENTS.md\n")];
        let cases = [
            Case {
                label: ".claude/CLAUDE.md @AGENTS.md resolves beside the importer",
                cwd: ".",
                agents: "AGENTS.md",
                files: &beside,
                expect_note: true,
                import_hint: Some("`@../AGENTS.md`"),
            },
            Case {
                label: ".claude/CLAUDE.md @../AGENTS.md loads the project file",
                cwd: ".",
                agents: "AGENTS.md",
                files: &parent_import,
                expect_note: false,
                import_hint: None,
            },
            Case {
                label: "mid-sentence @AGENTS.md is an import",
                cwd: ".",
                agents: "AGENTS.md",
                files: &mid_sentence,
                expect_note: false,
                import_hint: None,
            },
            Case {
                label: "fenced @AGENTS.md is not an import",
                cwd: ".",
                agents: "AGENTS.md",
                files: &fenced,
                expect_note: true,
                import_hint: Some("`@AGENTS.md`"),
            },
            Case {
                label: "parent @AGENTS.md resolves to the parent file, not app/AGENTS.md",
                cwd: "app",
                agents: "app/AGENTS.md",
                files: &parent_dir,
                expect_note: true,
                import_hint: Some("`@app/AGENTS.md`"),
            },
        ];
        let home = tempfile::tempdir().unwrap();
        for case in cases {
            let root = tempfile::tempdir().unwrap();
            for (rel, body) in case.files {
                let path = root.path().join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, body).unwrap();
            }
            let cwd = root.path().join(case.cwd);
            let agents = root.path().join(case.agents);
            std::fs::create_dir_all(agents.parent().unwrap()).unwrap();
            if !agents.is_file() {
                std::fs::write(&agents, "# agents\n").unwrap();
            }
            let note =
                claude_code_skips_agents_md(std::slice::from_ref(&agents), &cwd, Some(home.path()));
            if case.expect_note {
                let note = note.unwrap_or_else(|| panic!("{}: expected a note", case.label));
                assert!(
                    note.contains("so Claude Code reads it instead of AGENTS.md"),
                    "{}: {note}",
                    case.label
                );
                if let Some(hint) = case.import_hint {
                    let shown = hint.replace('/', std::path::MAIN_SEPARATOR_STR);
                    assert!(
                        note.contains(hint) || note.contains(&shown),
                        "{}: note {note} does not name {hint}",
                        case.label
                    );
                }
            } else {
                assert!(
                    note.is_none(),
                    "{}: unexpected note: {}",
                    case.label,
                    note.unwrap_or_default()
                );
            }
        }
    }

    /// Code spans are not imports, and `~/` resolves from the home directory
    /// rather than from the importing file (#1298).
    #[test]
    fn claude_code_import_skips_code_spans_and_expands_home() {
        let root = tempfile::tempdir().unwrap();
        let agents = root.path().join("AGENTS.md");
        std::fs::write(&agents, "# agents\n").unwrap();
        std::fs::write(
            root.path().join("CLAUDE.local.md"),
            "See `@AGENTS.md` for the syntax.\n",
        )
        .unwrap();
        let unrelated_home = tempfile::tempdir().unwrap();
        let note = claude_code_skips_agents_md(
            std::slice::from_ref(&agents),
            root.path(),
            Some(unrelated_home.path()),
        );
        assert!(
            note.as_deref()
                .is_some_and(|text| text.contains("`@AGENTS.md`")),
            "a code span is not an import: {}",
            note.unwrap_or_default()
        );

        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let agents = project.join("AGENTS.md");
        std::fs::write(&agents, "# agents\n").unwrap();
        std::fs::write(project.join("CLAUDE.local.md"), "@~/proj/AGENTS.md\n").unwrap();
        let note =
            claude_code_skips_agents_md(std::slice::from_ref(&agents), &project, Some(home.path()));
        assert!(
            note.is_none(),
            "home import of the written file should suppress the note: {}",
            note.unwrap_or_default()
        );

        std::fs::write(project.join("CLAUDE.local.md"), "@~/other/AGENTS.md\n").unwrap();
        let note =
            claude_code_skips_agents_md(std::slice::from_ref(&agents), &project, Some(home.path()));
        assert!(
            note.is_some(),
            "a home import of a different file still leaves AGENTS.md unloaded"
        );
    }

    #[test]
    fn merge_appends_to_empty_file() {
        let out = merge_instructions_block("", "BLOCK\n");
        assert_eq!(out, "BLOCK\n");
    }

    #[test]
    fn merge_appends_when_no_markers_present() {
        let original = "# My project\n\nSome notes.\n";
        let out = merge_instructions_block(original, "BLOCK\n");
        assert!(out.starts_with("# My project"));
        assert!(out.ends_with("BLOCK\n"));
        // One blank line between user content and our block.
        assert!(out.contains("Some notes.\n\nBLOCK\n"));
    }

    /// Real-world contract: the caller passes a marker-wrapped
    /// block (that's what `run()` builds). The merge replaces the
    /// prior bracketed section in place.
    #[test]
    fn merge_replaces_existing_block() {
        let original =
            format!("# My project\n\n{MARKER_START}\nOLD\n{MARKER_END}\n\nMore notes.\n");
        let new_block = format!("{MARKER_START}\nNEW BLOCK\n{MARKER_END}\n");
        let out = merge_instructions_block(&original, &new_block);
        assert!(out.contains("# My project"));
        assert!(out.contains("NEW BLOCK"));
        // Old content gone.
        assert!(!out.contains("OLD"));
        // User content after the block is preserved.
        assert!(out.contains("More notes."));
        // No duplicate markers.
        assert_eq!(out.matches(MARKER_START).count(), 1);
        assert_eq!(out.matches(MARKER_END).count(), 1);
    }

    #[test]
    fn merge_idempotent_double_run() {
        let block = format!("{MARKER_START}\nBLOCK\n{MARKER_END}\n");
        let first = merge_instructions_block("# Title\n", &block);
        let second = merge_instructions_block(&first, &block);
        assert_eq!(first, second, "second merge must be a no-op");
    }

    /// Regression: a block whose body *mentions* the end marker inline
    /// (mid-line, as older snippets did between backticks) must not be
    /// truncated at that mention. The naive `find` matched the inline
    /// marker, cut early, and re-injected the tail on every refresh; the
    /// line-anchored matcher must keep the double-run a true no-op.
    #[test]
    fn merge_ignores_inline_marker_mention_in_block() {
        let block = format!(
            "{MARKER_START}\nsee the `{MARKER_END}` marker inline\nreal body\n{MARKER_END}\n"
        );
        let first = merge_instructions_block("# Title\n", &block);
        let second = merge_instructions_block(&first, &block);
        assert_eq!(first, second, "double-run must be a no-op");
        // One inline mention + one real delimiter — nothing accumulated.
        assert_eq!(second.matches(MARKER_START).count(), 1);
        assert_eq!(second.matches(MARKER_END).count(), 2);
    }

    #[test]
    fn merge_repairs_exact_legacy_orphan_tail() {
        let block = full_block();
        let legacy_corrupt = format!("# Title\n\n{block}{LEGACY_ORPHAN_TAIL_LF}More notes.\n");
        let out = merge_instructions_block(&legacy_corrupt, &block);
        assert_eq!(out, format!("# Title\n\n{block}More notes.\n"));
    }

    #[test]
    fn merge_repairs_repeated_legacy_orphan_tails() {
        let block = full_block();
        let legacy_corrupt = format!(
            "# Title\n\n{block}{LEGACY_ORPHAN_TAIL_LF}{LEGACY_ORPHAN_TAIL_CRLF}More notes.\n"
        );
        let out = merge_instructions_block(&legacy_corrupt, &block);
        assert_eq!(out, format!("# Title\n\n{block}More notes.\n"));
    }

    #[test]
    fn merge_repairs_exact_legacy_orphan_tail_crlf_variant() {
        let block = full_block();
        let legacy_corrupt = format!("# Title\r\n\r\n{block}{LEGACY_ORPHAN_TAIL_CRLF}More\r\n");
        let out = merge_instructions_block(&legacy_corrupt, &block);
        assert_eq!(out, format!("# Title\r\n\r\n{block}More\r\n"));
    }

    #[test]
    fn merge_consumes_crlf_after_end_marker() {
        let original = format!("# Title\r\n\r\n{MARKER_START}\r\nOLD\r\n{MARKER_END}\r\nMore\r\n");
        let block = format!("{MARKER_START}\nNEW\n{MARKER_END}\n");
        let out = merge_instructions_block(&original, &block);
        assert_eq!(out, format!("# Title\r\n\r\n{block}More\r\n"));
    }

    /// The real shipped block round-trips cleanly through a refresh.
    #[test]
    fn merge_idempotent_with_real_block() {
        let block = full_block();
        let first = merge_instructions_block("# Title\n", &block);
        let second = merge_instructions_block(&first, &block);
        assert_eq!(first, second, "refresh of the real block must be a no-op");
        assert_eq!(
            second.matches(MARKER_END).count(),
            block.matches(MARKER_END).count(),
            "no orphaned end marker accumulated"
        );
    }

    /// Defensive: existing file ends without trailing newline. We
    /// should still produce well-formed output.
    #[test]
    fn merge_tolerates_missing_trailing_newline() {
        let out = merge_instructions_block("# Title", "BLOCK\n");
        assert!(out.starts_with("# Title\n"));
        assert!(out.ends_with("BLOCK\n"));
    }
}
