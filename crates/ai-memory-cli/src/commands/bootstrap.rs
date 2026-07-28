//! `ai-memory bootstrap` — ingest an existing project's history.
//!
//! Thin HTTP client wrapper. Source collection (git log, README, docs/,
//! Rust module headers, project-rules files) happens locally via
//! `ai_memory_consolidate::collect_sources`; the resulting bundle is
//! POSTed to `POST /admin/bootstrap` on the running server, which does
//! the LLM call and wiki writes. The CLI never opens a `Store` or `Wiki`
//! directly.
//!
//! Required environment variables (see "Configuring the CLI" in README):
//! - `AI_MEMORY_SERVER_URL` — base URL of the running server.
//! - `AI_MEMORY_AUTH_TOKEN` — bearer token if the server has auth enabled.

use ai_memory_consolidate::{
    BootstrapOutcome, BootstrapSource, SourceCounts, collect_sources, discover_repo_root,
    prune_sources_to_budget,
};
use anyhow::{Context, Result};
use tracing::info;

use crate::cli::BootstrapArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Run the `bootstrap` subcommand.
///
/// Collects sources locally from the project repo, then POSTs the
/// bundle to the server's `POST /admin/bootstrap` endpoint.
///
/// # Errors
/// Bails when the resolved repo path cannot be inspected, when source
/// collection fails, or when the server returns a non-2xx response.
pub async fn run(config: &Config, args: BootstrapArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    info!(server = %ep.url, auth = ep.auth_token.is_some(), "bootstrap CLI configured");

    // ---- repo path — auto-detect via libgit2, fall back to CWD ----
    // Try libgit2's `Repository::discover` (walks up looking for
    // `.git`). If the user passed `--repo-path` explicitly, use it
    // unchanged. If the auto-detect finds a git repo, use its root.
    // If neither, fall back to the current working directory and
    // silently disable git-history collection — README, docs/ and
    // project-rules files are still useful seeds even without a
    // git history.
    let (repo_path, has_git) = match args.repo_path {
        Some(p) => {
            let has_git = p.join(".git").exists();
            (p, has_git)
        }
        None => match discover_repo_root(std::path::Path::new(".")) {
            Ok(root) => (root, true),
            Err(_) => {
                let cwd = std::env::current_dir()
                    .context("getting CWD for bootstrap (no git repo, falling back to .)")?;
                info!(
                    cwd = %cwd.display(),
                    "no .git found at or above CWD; bootstrapping from non-git sources only"
                );
                (cwd, false)
            }
        },
    };
    // When there's no git repo, force-disable git-commit collection
    // regardless of the user's --exclude-git flag. `collect_sources`
    // would otherwise try to open the repo and fail.
    let include_git = !args.exclude_git && has_git;
    if !has_git && !args.exclude_git {
        eprintln!(
            "note: no .git found at {}; bootstrapping from README/docs/rules only \
             (git-commit history skipped). Pass --repo-path or `git init` to include commits.",
            repo_path.display()
        );
    }

    // ---- project — auto-derive from repo basename if absent -------
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    info!(workspace = %workspace, project = %project, repo_path = %repo_path.display(), git = has_git, "bootstrap target");

    // ---- collect sources locally ----------------------------------
    let sources = collect_sources(
        &repo_path,
        args.since.as_deref(),
        include_git,
        !args.exclude_readme,
        !args.exclude_docs,
        !args.exclude_code,
    )?;
    let collected = sources.len();
    let (sources, dropped, est_tokens) = prune_sources_to_budget(sources, args.max_input_tokens);
    let chunk_budget = ai_memory_consolidate::effective_chunk_budget(
        args.chunk_input_tokens,
        args.max_input_tokens,
    );
    let llm_chunks =
        ai_memory_consolidate::plan_bootstrap_chunks(sources.clone(), chunk_budget).len();
    info!(
        collected,
        kept = sources.len(),
        dropped,
        est_tokens,
        llm_chunks,
        chunk_budget,
        "pruned sources to token budget before POST (avoids 413 on large repos)"
    );

    // ---- short-circuit when --dry-run -----------------------------
    // The previous flow POSTed the full source bundle to the server
    // even in dry-run mode, which (a) defeated the purpose of the
    // local preview and (b) exploded with 413 on repos large enough
    // to exceed the server's 10 MB body limit. We now compute the
    // dry-run summary entirely client-side and skip the round-trip.
    //
    // Server parity: `process_sources` returns `NoSources` when the
    // collected bundle is empty, so the operator catches a wrong
    // repo path or over-broad `--exclude-*` flag before they think
    // they bootstrapped something. Mirror that here.
    if args.dry_run {
        ensure_sources_for_dry_run(&sources)?;
        let outcome = local_dry_run(
            sources,
            collected,
            args.max_input_tokens,
            args.chunk_input_tokens,
        );
        print_human_report(&outcome, &workspace, &project);
        let report = serde_json::to_string_pretty(&outcome)?;
        println!("\n--- machine-readable ---\n{report}");
        return Ok(());
    }

    // ---- POST to server -------------------------------------------
    let body = serde_json::json!({
        "workspace": workspace,
        "project": project,
        "sources": sources,
        "sources_collected": collected,
        "max_input_tokens": args.max_input_tokens,
        "chunk_input_tokens": args.chunk_input_tokens,
        "dry_run": args.dry_run,
        "force": args.force,
    });
    let outcome: BootstrapOutcome = post_json(&ep, "/admin/bootstrap", &body).await?;

    print_human_report(&outcome, &workspace, &project);
    let report = serde_json::to_string_pretty(&outcome)?;
    println!("\n--- machine-readable ---\n{report}");
    Ok(())
}

/// Mirror the server's `process_sources` `NoSources` check so a local
/// dry-run on an empty bundle fails the same way a real bootstrap
/// would. Catches the operator who pointed at a wrong path, a folder
/// without a git repo / README / docs, or who combined `--exclude-*`
/// flags so aggressively that every source dropped — before they
/// think they bootstrapped something and waste tokens against a real
/// run later.
fn ensure_sources_for_dry_run(sources: &[BootstrapSource]) -> Result<()> {
    anyhow::ensure!(
        !sources.is_empty(),
        "no input sources collected (matches server `NoSources`): wrong repo path, or every \
         --exclude-* flag combined to drop everything. Remove at least one --exclude-* flag \
         or point --repo-path at a directory with a README, docs/, or commits.",
    );
    Ok(())
}

/// Build a `BootstrapOutcome` entirely client-side: mirrors what the
/// server would compute under `dry_run = true` (prune to budget, count
/// kinds, estimate tokens) without making the HTTP request. Used to
/// short-circuit the network round-trip when the user only wants a
/// preview of what would be sent.
fn local_dry_run(
    sources: Vec<BootstrapSource>,
    sources_collected: usize,
    max_input_tokens: usize,
    chunk_input_tokens: usize,
) -> BootstrapOutcome {
    let (kept, _dropped, est_tokens) = prune_sources_to_budget(sources, max_input_tokens);
    let kept_counts = SourceCounts::from_sources(&kept);
    let chunk_budget =
        ai_memory_consolidate::effective_chunk_budget(chunk_input_tokens, max_input_tokens);
    let llm_chunks = ai_memory_consolidate::plan_bootstrap_chunks(kept.clone(), chunk_budget).len();
    BootstrapOutcome {
        sources_collected,
        sources_sent: kept.len(),
        sources_dropped: sources_collected.saturating_sub(kept.len()),
        sources_by_kind: kept_counts,
        estimated_input_tokens: est_tokens,
        pages_written: Vec::new(),
        rationale: "(dry-run; LLM not invoked, no network round-trip)".to_string(),
        dry_run: true,
        llm_chunks,
    }
}

/// Render the bootstrap outcome as a human-friendly summary. Lists
/// each source kind separately + every page written + an explicit
/// "what ai-memory knows now" footer so the operator doesn't assume
/// the wiki has 100% coverage of the project.
fn print_human_report(outcome: &BootstrapOutcome, workspace: &str, project: &str) {
    let kind = if outcome.dry_run {
        "Dry-run"
    } else {
        "Bootstrap"
    };
    println!("\n{kind} complete for {workspace}/{project}\n");

    println!("Sources loaded into the LLM:");
    let c = &outcome.sources_by_kind;
    if c.git_commits > 0 {
        println!(
            "  - {} git commit summar{}",
            c.git_commits,
            if c.git_commits == 1 { "y" } else { "ies" }
        );
    }
    if c.readme > 0 {
        println!("  - README");
    }
    if c.doc_files > 0 {
        println!(
            "  - {} doc file{} (under docs/)",
            c.doc_files,
            if c.doc_files == 1 { "" } else { "s" }
        );
    }
    if c.module_headers > 0 {
        println!(
            "  - {} Rust module header{}",
            c.module_headers,
            if c.module_headers == 1 { "" } else { "s" }
        );
    }
    if c.project_rules > 0 {
        println!(
            "  - {} project-rules file{} (CLAUDE.md / AGENTS.md / ...)",
            c.project_rules,
            if c.project_rules == 1 { "" } else { "s" }
        );
    }
    println!(
        "  -> ~{} input tokens estimated{}",
        outcome.estimated_input_tokens,
        if outcome.sources_dropped > 0 {
            format!(
                " (dropped {} lower-priority source{} to stay under budget)",
                outcome.sources_dropped,
                if outcome.sources_dropped == 1 {
                    ""
                } else {
                    "s"
                }
            )
        } else {
            String::new()
        }
    );
    if outcome.llm_chunks > 1 {
        println!(
            "  -> {} sequential LLM chunk{}",
            outcome.llm_chunks,
            if outcome.llm_chunks == 1 { "" } else { "s" }
        );
    }

    if outcome.dry_run {
        println!("\n(dry-run -- no LLM call, no pages written)");
    } else {
        println!(
            "\nGenerated {} wiki page{}:",
            outcome.pages_written.len(),
            if outcome.pages_written.len() == 1 {
                ""
            } else {
                "s"
            }
        );
        for p in &outcome.pages_written {
            println!("  - {p}");
        }
        if !outcome.rationale.is_empty() {
            println!("\nRationale: {}", outcome.rationale);
        }
    }

    println!(
        "\nWhat ai-memory knows now\n  \
         Only the sources listed above. NOT every file in your project,\n  \
         NOT every commit since project start, NOT runtime behaviour or\n  \
         test logs. As you use Claude Code (or another MCP agent) the\n  \
         lifecycle hooks will automatically capture your actual workflow,\n  \
         and consolidation will refine the wiki over time."
    );
}

#[cfg(test)]
mod tests {
    use super::{ensure_sources_for_dry_run, local_dry_run};
    use ai_memory_consolidate::{BootstrapSource, SourceKind};

    fn source(kind: SourceKind, label: &str, body_len: usize) -> BootstrapSource {
        BootstrapSource {
            kind,
            label: label.to_string(),
            text: "x".repeat(body_len),
        }
    }

    #[test]
    fn local_dry_run_marks_outcome_as_dry_run() {
        // The CLI handler refuses to call `local_dry_run` with zero
        // sources (server parity — see `anyhow::ensure!` in `run`),
        // so the lowest input we can hand the helper is one source.
        let outcome = local_dry_run(
            vec![source(SourceKind::Readme, "README", 50)],
            1,
            150_000,
            ai_memory_consolidate::DEFAULT_CHUNK_INPUT_TOKENS,
        );
        assert!(outcome.dry_run);
        assert_eq!(outcome.sources_collected, 1);
        assert_eq!(outcome.sources_sent, 1);
        assert!(outcome.pages_written.is_empty());
        assert!(outcome.rationale.contains("dry-run"));
    }

    #[test]
    fn local_dry_run_tallies_kinds() {
        let sources = vec![
            source(SourceKind::Readme, "README", 200),
            source(SourceKind::GitCommit, "feat: x", 100),
            source(SourceKind::GitCommit, "fix: y", 100),
            source(SourceKind::DocFile, "docs/a.md", 300),
        ];
        let outcome = local_dry_run(
            sources,
            4,
            150_000,
            ai_memory_consolidate::DEFAULT_CHUNK_INPUT_TOKENS,
        );
        assert_eq!(outcome.sources_collected, 4);
        assert_eq!(outcome.sources_sent, 4);
        assert_eq!(outcome.sources_dropped, 0);
        assert_eq!(outcome.sources_by_kind.readme, 1);
        assert_eq!(outcome.sources_by_kind.git_commits, 2);
        assert_eq!(outcome.sources_by_kind.doc_files, 1);
        assert!(outcome.estimated_input_tokens > 0);
    }

    #[test]
    fn ensure_sources_for_dry_run_rejects_empty() {
        // Server's `process_sources` returns `BootstrapError::NoSources`
        // on an empty bundle (status 422). The client-side dry-run path
        // must do the same so wrong repo paths / over-broad excludes
        // don't get silently glossed over.
        let err = ensure_sources_for_dry_run(&[])
            .expect_err("empty bundle must fail in dry-run, matching server NoSources");
        let msg = format!("{err}");
        assert!(
            msg.contains("no input sources collected") && msg.contains("NoSources"),
            "error message should reference both the user-facing cause and the server parity: {msg}",
        );
    }

    #[test]
    fn ensure_sources_for_dry_run_accepts_non_empty() {
        // A single source is enough — paridade with the server's check,
        // which gates on emptiness alone (budget pruning happens later).
        ensure_sources_for_dry_run(&[source(SourceKind::Readme, "README", 50)])
            .expect("non-empty bundle must pass");
    }

    #[test]
    fn local_dry_run_drops_to_fit_tight_budget() {
        // Fabricate ~30 commits, each ~1 KB of text → ~7.5 K tokens total
        // (chars/4). Budget of 2 K tokens (minus ~1 K scaffolding reserve
        // → effective ~1 K) forces aggressive drops.
        let sources: Vec<_> = (0..30)
            .map(|i| source(SourceKind::GitCommit, &format!("commit {i}"), 1000))
            .collect();
        let outcome = local_dry_run(
            sources,
            30,
            2_000,
            ai_memory_consolidate::DEFAULT_CHUNK_INPUT_TOKENS,
        );
        assert!(
            outcome.sources_dropped > 0,
            "should drop at least one source under tight budget"
        );
        assert_eq!(
            outcome.sources_collected,
            outcome.sources_sent + outcome.sources_dropped
        );
        assert!(outcome.dry_run);
    }

    #[test]
    fn local_dry_run_preserves_pre_prune_collected_count() {
        let outcome = local_dry_run(
            vec![source(SourceKind::Readme, "README", 50)],
            10,
            150_000,
            ai_memory_consolidate::DEFAULT_CHUNK_INPUT_TOKENS,
        );
        assert_eq!(outcome.sources_collected, 10);
        assert_eq!(outcome.sources_sent, 1);
        assert_eq!(outcome.sources_dropped, 9);
    }
}
