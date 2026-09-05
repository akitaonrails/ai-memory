---
name: ai-memory-handoff
description: "Use this skill for any request whose goal is session continuity across agents or time: finding a pending handoff, resuming previous work, saving next-session context, wrapping up, or discarding a mistaken handoff. Trigger by semantic intent rather than exact wording."
---
<!-- ai-memory-managed: routing-skill -->

# ai-memory handoff

Use this skill for single-use cross-session handoffs. Handoffs are for the next agent, not durable project documentation.

## Tools in this cluster

- `memory_handoff_accept` consumes the pending handoff when the user asks where we left off and no already-fetched handoff block is visible.
- `memory_handoff_begin` creates a terse next-session handoff only when the user is wrapping up, ending the session, or explicitly asks to save context for the next session.
- `memory_handoff_cancel` expires a mistaken pending handoff by exact handoff id.

## Single-use handoff behavior

The SessionStart hook usually fetches and consumes any pending handoff before the agent sees its first prompt. If the current context already contains a pending handoff block, answer from that block directly. Do not call the accept tool again to find it in another project, because handoffs are single-use and the tool will normally return null after SessionStart consumed it.

If no pending handoff block is visible and the user asks where we left off, then use the accept tool with the client-aware project scope below.

## Creating a handoff

Create a handoff only at session end or when the user explicitly asks to save context for the next session. Do not use handoffs for status checks, briefings, project notes, or permanent memory. Keep the summary to two or three concise sentences, and put details in open questions and next steps bullets.

Lifecycle hooks already capture routine prompts and tool calls, so do not manually write a handoff just to record normal progress.

On a shared server, a handoff belongs to the operator who created it. Set `shared: true` only when the user explicitly wants any operator in the project to receive the baton; do not infer sharing from ordinary collaboration prose.

## Canceling a handoff

Cancel only when the user asks to discard a handoff or you created one by mistake. Use the exact handoff id returned by the begin tool. Cancellation is idempotent from the user's point of view, but it should still target only the known handoff.

Accept and cancel normally act only on the caller's own plus shared handoffs. `any_owner: true` is a root-only recovery action over another operator's context; use it only on an explicit user request.

## Project scope

Choose scope from the MCP client's identity support:

- **Session-aware MCP clients** that forward the real lifecycle-hook session id on every request should use automatic current-project routing. Omit `workspace`, `project`, and `cwd` for the current repository; pass explicit scope only when the user names a different project.
- **Static MCP clients** (including clients with lifecycle hooks but no bridge connecting that hook session id to MCP requests) must pass `workspace` and `project` together on every project-scoped call, including requests about this project, here, or our work. Read the exact names from the nearest `.ai-memory.toml` when it declares both. If it does not, obtain the names from the operator or server configuration; never guess them from a directory name and never rely on the server's last active project.

This rule applies only to project-scoped calls. For cross-project retrieval, `global=true` must omit `workspace`, `project`, and `scopes`. For a standing preference written with `scope: "global"`, omit `workspace` and `project`.
