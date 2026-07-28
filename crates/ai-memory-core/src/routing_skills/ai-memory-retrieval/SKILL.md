---
name: ai-memory-retrieval
description: "Use this skill for any request whose goal is read-only retrieval from ai-memory: project history, prior context, decisions, rules, gotchas, recent activity, full wiki pages, or status/briefing. Trigger by semantic intent rather than exact wording, including when ai-memory is not named."
---
<!-- ai-memory-managed: routing-skill -->

# ai-memory retrieval

Use this skill for read-only ai-memory lookups, catch-up, and applying remembered project knowledge before you design, debug, or edit.

## Tools in this cluster

- `memory_query` searches the current project's wiki for prior decisions, gotchas, procedures, rules, and session notes.
- `memory_recent` lists the most recently updated pages when the user wants a light activity check.
- `memory_read_page` fetches a full page body after a search hit or direct path lookup.
- `memory_status` reports whether ai-memory is healthy and how large the knowledge base is.
- `memory_briefing` returns a structured read-only snapshot for agent consumption.
- `memory_explore` returns a prose digest when the user asks for an open-ended catch-up.

## Scope default

Default to the current project. The tools auto-scope from the working directory, so omit project, workspace, and cwd arguments unless the user explicitly names a different project. Phrases like this project, here, we, our work, and where did we leave off mean the current project.

## Choose the smallest useful lookup

- Use the search tool when the user asks whether something was discussed, before proposing architecture, or before non-trivial coding in a subsystem with possible prior decisions.
- Before non-trivial coding, debugging, deployment, release, auth, scope, migration, PR review, or data-preservation work, search memory for the subsystem and task type first. If the first search is thin, broaden or query more specific subsystem/error terms before designing a fix.
- Use the recent-pages tool for a quick what changed lately view.
- Use the status tool only for health and size questions.
- Use the structured briefing when code needs counts, windows, pending-handoff counts, current rules, or recent pages as JSON-like data.
- Use the prose exploration tool for broad catch-up questions like what is important right now or I have been away.

## Broaden on miss

If a current-project search is empty or thin, do not conclude the knowledge was never recorded. It may live in a sibling project such as infra, ops, or a related app.

- If the user named the sibling project or you know the likely sibling, search explicit `scopes`, for example `scopes: [{ "workspace": "default", "project": "infra" }]`.
- If you do not know where it lives, search globally across every project with `global=true`.
- Do not combine `global=true` with `scopes`, `project`, or `workspace` arguments.

## Snippets are not full pages

Search returns snippets, not complete bodies. An empty-looking or short snippet does not prove the page is empty because the match can be outside the snippet window. Fetch the full page when the path or title looks relevant, especially for rules, procedures, decisions, and gotchas.

Search order combines relevance with a bounded source-authority adjustment.
Maintained rules, decisions, procedures, and gotchas normally beat closely
matching session evidence; explicitly historical or session-specific queries
can still return session pages because low-authority sources are downgraded,
not hidden. Do not treat `pinned` alone as proof that a page answers the query.

## Apply retrieved guidance

Treat matching pages under `_rules/`, `gotchas/`, `procedures/`, and `decisions/` as operating constraints.

- Apply rules as current project policy.
- Check gotchas before editing the same subsystem.
- Follow procedures as checklists for releases, PR review, deploys, migrations, and other repeatable workflows.
- Treat decisions as prior architecture unless the user asks to revisit them.
