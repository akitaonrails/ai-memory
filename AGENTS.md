<!-- ai-memory:start -->
## Long-term memory (ai-memory)

This project uses [ai-memory](https://github.com/akitaonrails/ai-memory)
for cross-session continuity. **Lifecycle hooks already capture every
prompt + tool call automatically.** You never need to manually write
routine notes; the SessionStart hook auto-fetches pending handoffs and
the SessionEnd hook auto-consolidates. Only write a durable wiki page
when the user explicitly asks to remember or annotate something
permanently.

### When to reach for each tool

The user can express any of the intents below in plain English —
match the intent to the tool. They do not need to name the tool.

| User says / situation | Tool |
|---|---|
| "have we discussed X?" / "search memory for Y" / before proposing architecture | `memory_query` |
| "what's been going on" / "show recent activity" (light) | `memory_recent` |
| "is ai-memory healthy?" / "how big is the wiki?" | `memory_status` |
| "give me the stats" / structured snapshot for the agent to consume | `memory_briefing` |
| "catch me up" / "I've been away" / "what's important right now?" / open-ended exploration | `memory_explore` |
| "where did we leave off?" — and you see a `📥 ai-memory: pending handoff` block in your context | already done — answer from that block; do NOT re-call `memory_handoff_accept` |
| "where did we leave off?" — and no such block is visible | `memory_handoff_accept` (rare; the SessionStart hook usually got there first) |
| "save context for the next session" / wrapping up | `memory_handoff_begin` (single-use handoff; terse summary; put detail in `open_questions` + `next_steps` bullets) |
| "consolidate this session" / "compile what we learned" (usually automatic) | `memory_consolidate` |
| "remember this permanently" / "save a note" / "add an annotation" / durable project knowledge | `memory_write_page` (write a wiki page; do **not** use handoff for permanent notes) |
| "audit the wiki" / "find contradictions" / "what rules should we add?" | `memory_lint` |
| "prune old pages" / "memory cleanup" | `memory_forget_sweep` |

`memory_explore` is the right default for the "I want to know what's
going on" use case — it returns a prose digest whose verbosity
scales automatically to how long it's been since the last activity
(< 1 h → one line; > 30 days → full catchup).

### When you write a project rule, write it here

If you're about to write a durable project rule ("always X", "never
Y", "all PRs must …"), this rules file (CLAUDE.md for Claude Code;
AGENTS.md for Codex / OpenCode / Cursor / Gemini CLI; whichever
convention your agent uses) is where it belongs. ai-memory's lint
pass surfaces the same hint automatically when a `kind: rule` page
lands in `_rules/`.

### Refreshing this snippet

This block is maintained by ai-memory. Two ways to refresh it with
the latest binary's recommended copy:

- **From the agent** (no terminal needed): ask "refresh the ai-memory
  routing in this project" — the agent calls
  `memory_install_self_routing`, picks the right filename for itself
  (Claude Code → `CLAUDE.md`; Codex / OpenCode / Cursor / Gemini →
  `AGENTS.md`), and uses its Write / Edit tool to land the block.
- **From the CLI**: `ai-memory install-instructions` (defaults to
  `CLAUDE.md`; pass `--target AGENTS.md` for non-Claude agents).

Both are idempotent: re-runs replace the block bracketed by
`<!-- ai-memory:start -->` / `<!-- ai-memory:end -->` markers
without disturbing the rest of the file.
<!-- ai-memory:end -->

## Project Maintenance Rules

- When a change affects user-visible behavior, installation, supported
  platforms, supported agents, providers, or deployment, update
  `CHANGELOG.md` and the README/docs support references in the same commit.
- Do not bump crate/package versions or cut release tags automatically after
  every merged PR. Ask the user before any version bump or release tag.
- When the MCP tool surface changes, update `MEMORY_INSTRUCTIONS`,
  `ai_memory_core::SNIPPET_BODY`, README/docs tool references, and regression
  tests that assert every tool appears in both prompt surfaces.

## Rust Engineering Rules

- Prefer small, behavior-preserving changes. Do not add compatibility
  branches, new abstractions, or new public surface unless a shipped caller,
  persisted data, or explicit requirement needs them.
- Optimize the real bottleneck class first: algorithm, query shape, batching,
  allocation count, IO boundaries, and container choice. Avoid clever
  micro-optimizations without evidence.
- Keep SQLite writes behind the single writer actor. For hot paths, batch work
  into one command/transaction instead of spawning many writer messages or
  opening per-row transactions.
- Avoid N+1 store reads. Prefer reader methods that return the data shape the
  caller actually needs, and use cached/prepared statements for repeated
  queries.
- Keep hook ingestion fire-and-forget and bounded. Do not introduce unbounded
  `tokio::spawn` fan-out, unbounded queues, or synchronous agent-facing waits.
- Keep CLI commands thin: parse arguments, resolve config once, call typed
  library functions, render output. Provider-specific behavior belongs in the
  provider module, not in command handlers.
- Treat typed boundaries as load-bearing: IDs, `PagePath`, `AgentKind`,
  sanitization, workspace/project resolution, and provider dialects should be
  parsed or normalized once and reused.
- Prefer explicit fallbacks over `unwrap`, `expect`, or `unreachable!` in
  runtime paths. Panics are acceptable in tests only.
- Do not use `unsafe` for performance work in this project unless profiling
  proves it is necessary and the safety argument is documented in the code.
- Add focused regression tests for bug fixes and behavior changes. For
  filesystem tests, use temp dirs or injected roots; never depend on the real
  user home directory being writable.
- Run the full local gate before claiming a Rust change is ready:
  `cargo fmt --check`, `git diff --check`,
  `TAILWIND_SKIP=1 cargo test --workspace`, and
  `TAILWIND_SKIP=1 cargo clippy --workspace --all-targets -- -D warnings`.
