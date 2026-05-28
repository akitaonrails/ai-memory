# Day-to-day usage

This page covers what happens after ai-memory is installed: handoffs,
compaction recovery, proactive memory queries, the web UI, and the
project-rule routing snippet.

## Cross-agent handoff

You normally do not create handoffs by hand. With lifecycle hooks
installed, session-end capture writes the handoff and the next
session-start hook fetches it.

```text
$ claude
> Working on the auth refactor. JWT rotation is broken; trying session cookies.
[work for an hour]
> /exit

$ codex   # in the same directory, later
[SessionStart hook fetches the handoff; Codex sees it before your prompt.]
> Picking up: you were investigating session cookies as an alternative...
```

If an agent has MCP but no lifecycle hook surface, ask it to call
`memory_handoff_begin` before quitting. The next hooked agent can still
consume that handoff automatically.

## Compaction recovery

When Claude Code or Codex compact their working context, the
`PreCompact` hook fires and ai-memory writes a fresh
`sessions/<id>.md` page summarising the session so far. After
compaction, the agent can recover the summary via `memory_recent` even
though its raw chat history was compacted away.

## Proactive memory queries

Hooks handle capture without prompting. Proactive querying depends on
the agent knowing which MCP tool to call for each situation. Install the
routing snippet into the project rules file once, then agents can use
the wiki without you naming tools explicitly.

| You say | Agent calls | Effect |
|---|---|---|
| "Have we discussed X?" / "search memory for Y" | `memory_query` | FTS5 + graph/vector RRF over compiled wiki pages, with bounded raw-observation fallback. |
| Before proposing architecture | `memory_query` | Checks prior decisions and gotchas before suggesting designs. |
| "Catch me up" / "I've been away" | `memory_explore` | Prose digest whose verbosity scales with time since last activity. |
| "Where did we leave off?" | Existing handoff block, or `memory_handoff_accept` if no block exists | Resumes from the latest pending handoff. |
| "Save context for the next session" | `memory_handoff_begin` | Writes a terse handoff with open questions and next steps. |
| "Consolidate this session" | `memory_consolidate` | Manually runs LLM consolidation. Also runs on PreCompact, and at session end only when `AI_MEMORY_CONSOLIDATE_ON_SESSION_END` is set (off by default; session end otherwise writes a rule-based summary page). |
| "Remember this permanently" / "add an annotation" | `memory_write_page` | Writes durable wiki knowledge; not a single-use handoff. |
| "Audit the wiki" / "any contradictions?" | `memory_lint` | Runs stale-page, contradiction, and rule-suggestion checks. |
| "How big is the wiki?" / "stats?" | `memory_status`, `memory_briefing` | Counts and recent activity windows. |

## Install the routing snippet

From an agent, say:

```text
Install ai-memory routing into this project.
```

The agent calls `memory_install_self_routing`, receives the canonical
snippet, and writes it to the right rules file (`CLAUDE.md` for Claude
Code, `AGENTS.md` for Codex / OpenCode / Cursor / Gemini CLI /
Antigravity CLI). The
block is wrapped in `<!-- ai-memory:start -->` and
`<!-- ai-memory:end -->`, so re-runs replace it in place.

From a terminal:

```bash
ai-memory install-instructions
ai-memory install-instructions --target AGENTS.md
ai-memory install-instructions --print
```

Auto-detect extends `CLAUDE.md` when it exists, `AGENTS.md` when it
exists, both when both exist, or creates `CLAUDE.md` when neither
exists. Use `--target AGENTS.md` for non-Claude-only projects.

## Bootstrap an existing project

If you install ai-memory into a project that already has months of
history, the wiki starts empty. `ai-memory bootstrap` seeds it from the
existing repo history and docs.

```bash
export AI_MEMORY_SERVER_URL="http://localhost:49374"
ai-memory bootstrap --dry-run
ai-memory bootstrap
```

The bootstrap collector reads `git log`, the root README, `docs/`,
project rule files, and Rust module docs, then POSTs the selected
sources to the running server. It requires an LLM provider on the
server. See [Installation cookbook - bootstrap mid-project](install.md#bootstrap-mid-project)
for flags, token budgets, and source priority.

If you use the ChatGPT/Codex OAuth provider, sign in once before starting the
server with `AI_MEMORY_LLM_PROVIDER=openai-oauth`:

```bash
ai-memory auth login openai-oauth
ai-memory auth status
```

The login command stores only provider credentials in `<data_dir>/auth.json`.
It is separate from `AI_MEMORY_AUTH_TOKEN`, which protects MCP, hooks, and the
web UI.

For GitHub Copilot, use the matching provider login before starting the server
with `AI_MEMORY_LLM_PROVIDER=copilot`:

```bash
ai-memory auth login copilot
ai-memory auth status
```

Copilot auth stores a GitHub user token, then the provider exchanges it for a
short-lived Copilot API token before each LLM call.

## Browse the wiki in a browser

Start the server with `--enable-web` and open
`http://<host>:49374/web`.

```bash
ai-memory serve --transport http --bind 127.0.0.1:49374 --enable-web
```

Docker compose users can add the flag to the service command:

```yaml
command: ["serve", "--transport", "http", "--bind", "0.0.0.0:49374", "--enable-web"]
```

The web UI is read-only: project list, per-project page tree,
breadcrumbs, rendered markdown, metadata, and FTS5 search. If the
server has `AI_MEMORY_AUTH_TOKEN` set, the browser uses HTTP Basic auth:
leave the username blank and paste the token as the password. MCP and
hook clients continue to use `Authorization: Bearer <token>`.

![Project list homepage with four projects shown as cards with page counts and last activity.](web-projects-home.png)

![Project view with folder tree, kind badges, and recent activity.](web-project-view.png)

## Inspect the raw wiki

The wiki is plain markdown plus git history.

```bash
docker exec ai-memory ls /data/wiki/sessions/
docker exec ai-memory cat /data/wiki/sessions/<uuid>.md

# Open in Obsidian or any markdown viewer:
docker cp ai-memory:/data/wiki ./my-ai-memory-wiki

# Time-travel:
docker exec ai-memory git -C /data/wiki log --oneline
```

## Rules vs facts

Durable project rules belong in the agent's rules file, not only in the
wiki. For Claude Code that is `CLAUDE.md`; for Codex, OpenCode,
Cursor, and Gemini CLI it is usually `AGENTS.md`.

The consolidator classifies compiled observations as `decision`,
`fact`, `rule`, or `gotcha`. Rule-tagged pages are routed to
`wiki/_rules/<slug>.md`, and `memory_lint` reports a suggestion when a
rule looks durable enough to copy into `CLAUDE.md` or `AGENTS.md`.

ai-memory never edits the rules file on its own. The lint suggestion is
the whole workflow: copy the rule if it should apply every turn, ignore
it if it was temporary context.
