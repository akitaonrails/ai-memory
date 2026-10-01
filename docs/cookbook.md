# ai-memory cookbook

A task-oriented cheat sheet: "I want to do X" → how. For the full reference see
[`ARCHITECTURE.md`](ARCHITECTURE.md); for install see [`install.md`](install.md)
(including the [macOS menu bar app](install.md#macos-menu-bar-app));
for the tool-routing table see [`usage.md`](usage.md).

## What ai-memory is, in one paragraph

ai-memory gives your AI coding agents long-term, cross-session memory. It runs
as one server; your agent talks to it over MCP (tools like `memory_query`,
`memory_write_page`) and over lifecycle hooks that automatically capture what you
do. Memory is a **markdown wiki in git** (the source of truth, hand-editable) plus
a derived SQLite index for search. Everything is **scoped per project**
`(workspace, project)`, resolved from your working directory. It works with no
LLM at all (capture + full-text search + rule-based summaries); adding a provider
enables consolidation and auto-improvement.

## Recipe: use ai-memory as your tool's memory

Call the existing MCP tools to save pages, query knowledge and pass a handoff
between executions. Native hooks are optional. The [programmatic memory guide](programmatic-memory.md)
includes complete HTTP requests, scope rules and machine authentication.
If your tool also hosts a harness, follow its optional lifecycle-capture section.

## Everyday tasks (through your agent, over MCP)

You mostly just talk to your agent; it calls the right tool. Common ones:

| You want | Say to your agent | Tool it uses |
|---|---|---|
| Recall prior work | "Have we discussed X?" / "what did we decide about Y?" | `memory_query` |
| Catch up after time away | "Where did we leave off?" / "catch me up" | handoff block / `memory_explore` |
| Remember a durable fact | "Remember that we always use pnpm here" | `memory_write_page` |
| Remember until a date | "Remember this until the migration ships" | `memory_write_page` + `expires_at` |
| A standing rule for *every* project | "Always: never force-push" | `memory_write_page` with `scope: "global"` |
| Read a specific saved page in full | "Read the `_rules/deploy` page" | `memory_read_page` |
| Save context for the next session | "Wrap up — save context for next time" | `memory_handoff_begin` |
| See project stats | "How much memory do we have?" | `memory_status` / `memory_briefing` |

You rarely write pages by hand: lifecycle hooks capture prompts and tool calls
automatically, and (with an LLM) consolidation compiles them into pages.

## Recipe: keep a rule a project must follow

> "Remember: before touching the payment code, read the PCI notes."

Your agent calls `memory_write_page` and it lands as a durable wiki page (routed
under `_rules/` when it's a rule). Next session, `memory_query` surfaces it, and
if the project's `.ai-memory.toml` opts into the on-start brief, rules are
prepended to the agent's context automatically. To make it apply to **all** your
projects, ask for it as a global rule (`scope: "global"`).

## Recipe: have a project read a specific document before implementing

Two ways, depending on where the document lives:

1. **It's already a wiki page** (you saved it, or imported it — below): tell the
   agent "read the `norms/gdpr-retention` page before implementing" — it calls
   `memory_read_page` with that path and works from the full body.
2. **It's an external file** (a norm/spec on disk): save it as a page first
   ("remember this spec as `norms/gdpr-retention`", paste or point at it), then
   reference it as in (1). Large references are best split into a page per
   document so retrieval can pull just the relevant one.

## Recipe: import an existing knowledge base (e.g. OKF norms/laws/specs)

ai-memory's wiki **is** OKF (Obsidian-compatible markdown + frontmatter). To
bring an existing body of documents in as project memory:

- **A few documents**: save each as a durable page via your agent
  (`memory_write_page`) or the CLI (`ai-memory write-page`), one page per
  document, under a stable prefix like `norms/…`. Then any project can read a
  specific one before implementing (recipe above).
- **A whole OKF/Obsidian vault or an export from another tool**: use the
  companion importer (see [`companion-crates.md`](companion-crates.md)) — it
  ingests OMC and external-conversation exports into the store. `ai-memory
  export-okf` is the inverse (export your wiki as an OKF bundle), useful to see
  the exact on-disk shape your documents should take.
- The wiki lives at `<data_dir>/wiki/<workspace>/<project>/…` and is plain
  markdown in git, so you can also drop files in and let the file watcher +
  reindex pick them up. Keep them under a clear prefix and commit.

Once the material is saved as pages in the right scope, any project can search
it (`memory_query`) and read a specific document in full (`memory_read_page`)
before implementing against it.

## Recipe: control what gets kept, aged, or consolidated

By default nothing you have to think about: memory decays on a single gentle
curve, and an upgrade to 2.4 changes no scores and evicts nothing. When you *do*
want to tune aging, it is all opt-in and reversible — the original of anything
compacted or merged stays in git and the supersession chain, recoverable with
`ai-memory restore-page`.

- **Keep something forever:** pin it. A pinned page is exempt from the
  forget-sweep regardless of tier or age. Semantic and procedural pages never
  decay either — only working/episodic memory ages.
- **Make a note expire on a deadline:** ask your agent to remember it "until
  <date>" (an `expires_at`); the forget-sweep deletes it when the time passes,
  no matter how often it was read. A TTL outranks pinning.
- **Tune how long each tier lasts:** set per-tier half-lives in the project's
  `.ai-memory.toml` `[decay.half_life_days]` (e.g. keep episodic history longer,
  working-tier scratch shorter). Omit it for the default single curve.
- **Compact instead of evict, and de-duplicate:** `[decay] compact_cold_episodic`
  keeps a cold page's durable facts (paths, error codes, decisions) and drops the
  prose; `[decay] dedup_cold_clusters` collapses near-duplicate cold pages into
  one survivor. Both are zero-LLM, off by default, and supersede rather than
  delete.
- **Keep used memory longer:** nothing to configure — a page you open, search,
  or reach through a related-pages walk is reinforced automatically and resists
  decay.
- **Surface likely contradictions:** run `memory_lint` (through your agent or the
  CLI); with embeddings configured it flags pairs of same-topic pages that look
  like they conflict, advisory only. Similarity reads shared vocabulary as much
  as disagreement, so a single-domain or single-language store yields mostly
  candidate pairs: read each finding as a pair to check, not as a defect. If
  that noise is too high, raise `contradiction_band_min` (`config.toml` or
  `AI_MEMORY_CONTRADICTION_BAND_MIN`, default `0.4`) — on such a store the
  band measures domain proximity more than conflict, so a higher floor trims
  same-domain-but-unrelated pairs.
- **Let an LLM consolidate on idle ("dream"):** with a provider *and* an embedder
  configured, `[dream] enabled` turns on a background pass that rewrites clusters
  of cold notes into single coherent pages while you're idle and cancels the
  moment you return. It is off by default, never deletes a source, and is gated on
  an internal recall eval before it could ever become default behavior.

## Recipe: two agents / two repos working together

- **Continuity across agents in the same project** (quit Claude Code, open Codex
  in the same repo): automatic. A handoff is captured at session end and the
  next session's on-start hook prepends it. Ask "where did we leave off?".
- **Ask an agent in another project to do something** without loading that
  project's context here: cross-project messaging — "send project-b a request to
  add the export endpoint" (`memory_message_send`), and over there "check my
  inbox" (`memory_message_pop`). See [`agent-messaging.md`](agent-messaging.md).

## Recipe: several accounts or an external launcher

Give each account or provider its own config home and pass it with `--env`, so
the harness, its hooks and MCP, and transcript import all use that one home:

```bash
ai-memory run --env CLAUDE_CONFIG_DIR="$HOME/.claude-work" claude
ai-memory run --env CODEX_HOME="$HOME/.codex-work" codex   # the dir must exist
```

- `--env` and `--env-file` belong to `ai-memory run`, so they go before the
  harness name.
- A launcher or orchestrator that starts harnesses with its own environment
  (provider keys, an account's config dir) can write it to a file and pass
  `--env-file <path>`, one `KEY=VALUE` per line. Values are taken literally, so
  use absolute paths in the file.
- Auto-wire runs once per config home, so the second account gets its hooks +
  MCP on its own first launch. To wire one by hand, export the variable for the
  installers: `CLAUDE_CONFIG_DIR="$HOME/.claude-work" ai-memory install-hooks
  --agent claude-code --apply`, then the same for `install-mcp --client
  claude-code --apply`.

See [`managed-workstreams.md`](managed-workstreams.md).

## Recipe: run an unsupervised agent safely (`--yolo` + ai-jail)

`--yolo` maps to each harness's dangerous-mode flag (Claude Code →
`--dangerously-skip-permissions`), which runs every tool call with no
confirmation. `ai-memory run --yolo` adds guardrails around that, gated
entirely on a real interactive terminal (stdin and stderr both TTYs), so
scripts, hooks, and CI are never prompted:

```bash
ai-memory run --yolo claude
```

- **The warning.** Before the agent spawns, you are asked to confirm:
  `Enter`/`y`/`yes` proceeds (the default); `n`/`no` aborts before anything
  launches.
- **The ai-jail offer.** If [ai-jail](https://github.com/akitaonrails/ai-jail)
  is usable — on Linux/macOS, installed on `PATH` (or `~/.local/bin/ai-jail`),
  with its sandbox backend present (`bwrap` on Linux, `sandbox-exec` on
  macOS) — and you are not already inside it, a second question offers to
  re-run the session inside it. When it is not usable (or on Windows) there is
  no second question; the run just proceeds. Accepting re-execs the original
  command under `ai-jail --network --agent-state --env <NAME>... --`,
  forwarding only the credential/config
  environment variables that are already set (server/hook URL,
  `CLAUDE_CONFIG_DIR`, provider API keys, etc.) — `--network` keeps the
  loopback ai-memory server reachable while still sandboxing the filesystem.
  Declining keeps the run unsandboxed (your choice, already warned).
- **Already inside ai-jail.** Both prompts are skipped and the run proceeds
  directly — `ai-jail ai-memory run … --yolo` sees no extra friction.
  Detection is Linux (`ai-sandbox` hostname) / macOS (`PS1` starting with
  `(jail) `); it fails open (shows the warning) when undetectable, never
  open to skipping it silently.
- **Claude "true yolo".** `--true-yolo` includes everything `--yolo` does
  (`ai-memory run claude --true-yolo` is enough; adding `--yolo` too is
  harmless) and, for Claude, also forces `bypassPermissions` over any
  `defaultMode` in your settings. For other harnesses it is the same as
  `--yolo`. `claude_true_yolo = true` in `config.toml` /
  `AI_MEMORY_CLAUDE_TRUE_YOLO=true` applies the Claude extra to every `--yolo`
  launch, never to a run without it.
  **It cannot remove your own `ask` rules**: Claude Code honors explicit
  `permissions.ask` rules (and its built-in command-safety checks) in every
  mode, so a rule like `Bash(docker run *)` in `~/.claude/settings.json` still
  pauses the run. For a pause-free sandbox, drop those `ask` entries — `deny`
  rules block without pausing, so they can stay. Best paired with ai-jail.
- **Passing extra env, e.g. a GitHub token.** `ai-memory run claude --yolo
  --env GH_TOKEN="$(gh auth token)"` forwards it into the jailed agent (needs
  ai-jail 2.4.2 or later when you accept the jail offer). This
  hands a sandboxed agent your token, so only do it for work you'd trust it
  with; it is deliberately never forwarded automatically.

See [`design-yolo-safety-ai-jail.md`](design-yolo-safety-ai-jail.md) for the
full contract.

## Recipe: send different repositories to different servers

One machine, several organisations, each with its own ai-memory server.
Register each server once under a name, with the directory it is allowed to
serve, then let each repository's marker pick it:

```bash
ai-memory server add team-a --url https://memory-a.example.com --root ~/work/team-a --auth-token-stdin
ai-memory server add team-b --url https://memory-b.example.com --root ~/work/team-b --auth-token-stdin
```

```toml
# ~/work/team-b/.ai-memory.toml
workspace = "team-b"
server = "team-b"
```

Repositories without a `server` key keep using the server `install-hooks`
configured. The marker only ever names a profile; a profile that does not
resolve drops the event rather than sending it anywhere else. Details, the
fail-closed rules, and which integrations support it:
[`marker-file.md`](marker-file.md#routing-capture-to-another-server-server).

## Recipe: run the server on a Mac

Use the menu bar app when you want one `.app` that starts the server and
opens the existing tools (web UI, `ai-memory status`, config, logs). It does
not replace those tools with a second dashboard.

```bash
./companions/ai-memory-macos/build.sh
open "companions/ai-memory-macos/dist/AI Memory.app"
```

Drag **AI Memory.app** to `/Applications`, then **Install & Start Server**
from the menu extra. Wire an agent with the bundled binary:

```bash
BIN="/Applications/AI Memory.app/Contents/Resources/runtime/ai-memory"
"$BIN" install-mcp --client claude-code --apply
"$BIN" install-hooks --agent claude-code --apply
```

Memory stays in `~/Library/Application Support/ai-memory`. Replacing the
`.app` does not rewrite it. Full paths (tarball, source, Docker, launchd):
[`macos.md`](macos.md).

## From the terminal (CLI)

Your agent runs most of these for you; `run` and `continue` are how you start it:

```bash
ai-memory run <harness>              # launch a harness, hooks + MCP auto-wired
ai-memory continue                   # resume the newest managed checkout
ai-memory workstreams                # list this checkout's managed workstreams
ai-memory status                     # counts, paths, health
ai-memory doctor                     # is every harness that ran here captured?
ai-memory backfill                   # import prior local history into an empty store
ai-memory write-page …               # save a durable page
ai-memory handoffs                   # list open handoffs for the project
ai-memory message list               # cross-project inbox
ai-memory server list                # server profiles a marker can route to
ai-memory export-okf …               # export the wiki as an OKF bundle
ai-memory serve                      # run the server
```

## When it isn't doing what you expect

- **Nothing is being remembered**: hooks may not be installed. `ai-memory run
  <harness>` installs its hooks + MCP on the first launch per harness,
  ai-memory version and config home, and again after `ai-memory uninstall`. If that harness
  already launched through `run` and its hooks went missing another way
  (removed by hand, a failed first wire), install them by hand: `ai-memory
  install-hooks --agent <your-agent> --apply` and `ai-memory install-mcp
  --client <client> --apply`. Then check `ai-memory status` / `ai-memory doctor`.
- **Only *some* agents are being remembered**: run `ai-memory doctor`. It lists
  every harness that has local sessions in this project and whether the server
  captured them — so a harness you rotated in without installing its hook (a
  silent gap: it keeps its own local history while capturing nothing) shows up
  as a warning with the exact `install-hooks` command to fix it.
- **I just installed hooks in a project I've worked in for a while**: the first
  time you open the project after installing, ai-memory imports your existing
  local session history once (bounded, sanitized on the server, only into an
  empty store) so it isn't starting from nothing. It runs automatically in the
  background; you can trigger or preview it yourself with `ai-memory backfill`
  (`--dry-run` to see what it would import), or turn it off with
  `AI_MEMORY_BACKFILL_ON_START=false`.
- **A search misses something you saved**: confirm the scope — memory is
  per-project; a page saved in project A is not returned in project B unless it
  was written to the global scope or you query with an explicit scope.
- **You want an LLM feature (consolidation, digests)**: set a provider
  (`AI_MEMORY_LLM_PROVIDER=…`); without one, capture + search still work.
