---
name: ai-memory-durable-pages
description: "Use this skill for any explicit wiki mutation in ai-memory: saving durable or time-bounded project knowledge, recording a rule or annotation, updating a note, or deleting a memory page. Trigger by semantic intent rather than exact wording; routine session capture is not a durable-page request."
---
<!-- ai-memory-managed: routing-skill -->

# ai-memory durable pages

Use this skill only for deliberate durable wiki mutations. Routine session capture is automatic, and permanent notes require an explicit user request.

## Tools in this cluster

- `memory_write_page` writes a durable wiki page for permanent project knowledge.
- `memory_delete_page` removes a durable wiki page by exact path.

## Writing durable memory

Write a page only when the user explicitly asks to remember something permanently, save a note, add an annotation, or record project knowledge. Do not use durable pages for transient progress, normal status updates, or next-session context.

Put the page title as a `# H1` on the first line of the body and omit the separate title argument. ai-memory derives the title from that H1. Keep the content concise and fact-like, with enough context that a future agent can apply it without rereading the whole session.

Optional top-level metadata: `kind` (64 characters), `entities` (10 names,
64 characters each, normalized and deduplicated), `abstract` (1,024 characters),
and `relations` (an object mapping `causes`, `fixes`, or `contradicts` to page
target arrays, 32 targets total, 1,024 characters each). Relation targets use
`path`, `project:path`, or `workspace/project:path`; extensionless page paths
resolve with `.md`, and scope components must already be trimmed. Limits apply
to raw values before trimming, normalization or deduplication. Arbitrary
frontmatter and author fields are not editable.
Every write replaces the whole page and its metadata; omitted fields are
cleared. Read the current page first when preserving existing content matters.
There is no patch or compare-and-write precondition.

When the user explicitly wants a note to be temporary, pass `expires_at` as an
RFC3339 instant or a bare `YYYY-MM-DD` (the end of that day in UTC). After the
TTL, normal search, recent, and briefing reads hide the page; the next forget
sweep deletes it. A TTL outranks `pinned`, so do not combine them unless the user
has deliberately requested that behavior.

## Writing a session's pages yourself

When the user asks to consolidate the session you are taking part in, write its pages with your own model instead of the server's: read the session's raw observations first (the retrieval skill covers that read), then write each page with `memory_write_page` passing the same `session_id`, which records the session as evidence and settles its queued consolidation job. Match the server's multi-page layout, at most five pages:

- `sessions/<session_id>.md`: the session narrative, `tier: "episodic"`, `kind: "fact"`.
- `concepts/<slug>.md`: evergreen concept pages, `tier: "semantic"`, `kind: "fact"`.
- `decisions/<short>.md`: ADR-style records of a choice made, `tier: "semantic"`, `kind: "decision"`.
- `gotchas/<slug>.md`: failure modes and surprises, `tier: "semantic"`, `kind: "gotcha"`.
- `_rules/<slug>.md`: a durable project convention written as a standalone instruction, `tier: "semantic"`, `kind: "rule"`.

Only split out what the session actually established; a short session is one session page. Record what was said and done, and never turn a user's question into a confirmed decision.

## Project rules belong in instructions first

If the user asks to create a durable project rule such as always do X or never do Y, update the project's canonical agent instruction file when the repository says one exists. Use a durable page only when the user explicitly wants the rule in the wiki too, or when no canonical instruction file applies.

## Deleting durable memory

Delete only by exact path. If the user gives a vague title or topic, first resolve it to the page path using read-only lookup. Preserve sibling projects unless the user explicitly names them.

## Project scope

Choose scope from the MCP client's identity support:

- **Session-aware MCP clients** that forward the real lifecycle-hook session id on every request should use automatic current-project routing. Omit `workspace`, `project`, and `cwd` for the current repository; pass explicit scope only when the user names a different project.
- **Static MCP clients** (including clients with lifecycle hooks but no bridge connecting that hook session id to MCP requests) must pass `workspace` and `project` together on every project-scoped call, including requests about this project, here, or our work. Read the exact names from the nearest `.ai-memory.toml` when it declares both. Without a marker override, derive the project from normalized `upstream`, then `origin`, using the full repository path without its host (`github.com/acme/api` → `acme-api`); use the folder basename only when no valid remote exists. Never rely on the server's last active project.

This rule applies only to project-scoped calls. For cross-project retrieval, `global=true` must omit `workspace`, `project`, and `scopes`. For a standing preference written with `scope: "global"`, omit `workspace` and `project`.

## Architectural decisions get ADR structure and a pin

When the user asks to record an architectural decision (a chosen approach, a rejected alternative, a standing trade-off), write it as a durable page under `decisions/<short-slug>.md` with `pinned: true` and this structure in the body:

```markdown
# <Decision title>

**Status:** accepted   <!-- proposed | accepted | superseded by [[decisions/other]] -->

## Context
What situation forced a decision; the constraints that mattered.

## Decision
What was decided, stated as a fact.

## Consequences
What becomes easier, what becomes harder, what was given up.
Rejected alternatives and WHY, so future sessions don't re-propose them.
```

Pinned pages are exempt from retention decay and curation, and the auto-improvement path refuses to rewrite them — the record stays immutable unless a human unpins it. To supersede a decision, write a NEW page and set the old page's status line to `superseded by [[decisions/<new>]]`; never edit the old decision's substance. Note: ai-memory never touches files in the project repository — a `docs/adr/` directory managed there (by hand or by another tool) is outside ai-memory entirely; these wiki ADRs complement it for cross-session retrieval.

## Standing user/team preferences go to the cross-project profile

When the fact is a standing preference that should apply to EVERY project — technology choices ("always use pnpm workspaces"), code style ("prefer composition over inheritance"), personal conventions ("never `--force` without asking") — call `memory_write_page` with `scope: "profile"` instead of the current project. The page joins the cross-project profile under `profile/` (pass a path like `tools/pnpm.md`; it is stored as `profile/tools/pnpm.md`), every project's session start lists it as a default, and default memory reads surface it as `global_scope_hits`. Start the body with a `# Title` and one imperative line; pass `applies_to` (e.g. `["rust"]`) to scope it to projects on that stack, and `enforced_by` (e.g. `"pre-push hook"`) when something already enforces it, which keeps it out of the session-start digest; a rewrite that omits them clears them. Remove one with `memory_delete_page` and `scope: "profile"`. `scope: "global"` still writes the shared `_global` scope directly. Neither scope can be combined with workspace/project arguments. Use it only for genuinely cross-project preferences; project-specific rules stay in the project (or its instruction file, per the section above). A rules file always wins over the profile: the profile is the default for what the repository does not say.
