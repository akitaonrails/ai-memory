# Managed cross-harness workstreams

`ai-memory run` is an opt-in launcher that lets one logical coding session move
between Claude Code, Codex, OpenCode, OpenCode 2 beta, Pi, Crush, Kimi Code, Command Code, Kiro
CLI v2/v3, OMP, Grok Build CLI, Antigravity CLI, and GitHub Copilot CLI. Direct agent launches
keep their existing ai-memory behavior. There is no global mode toggle and no
`switch` command: using `run` selects the current workstream and transparently
creates or resumes the correct native session for the requested harness.

**`ai-memory run` is the preferred way to start a harness — "if in doubt, run
with ai-memory."** Beyond session continuity, the first time it launches a given
harness it **auto-installs that harness's ai-memory hooks and MCP** if they are
not already wired, so capture and recall work without a separate `install-hooks`
/ `install-mcp` step (a common footgun: `ai-memory run kimi` used to capture
nothing if the Kimi hooks were never installed). Auto-wire is idempotent and
one-time per harness, binary version and install location (a second config home,
such as another `CLAUDE_CONFIG_DIR`, gets its own first launch; `ai-memory
uninstall` of hooks or MCP clears that record, so the next launch wires again),
preserves
unrelated user config, runs
before the harness starts so it picks up the fresh hooks, and is best-effort —
if an install fails it warns and still launches. Harnesses without installer
support (Crush) are skipped; Pi wires hooks but has no MCP client to write. Turn
it off with `ai-memory run --no-autowire`, `AI_MEMORY_RUN_AUTOWIRE=false`, or
`run_autowire = false` in config; manual `install-hooks` / `install-mcp` remain
available for harnesses you never launch through `run`.

The launcher resolves its executable name through `PATH` directly — it does
not go through an interactive shell, so a `claude` defined only as a shell
`alias` in `.bashrc`/`.zshrc` is invisible to it. If you switch Claude
accounts by alias, put a same-named script or shim earlier on `PATH` instead
(or pass `--executable PATH`, which also resolves a bare name through
`PATH`), so the resolved `claude` process actually is the one you meant.

**Named launch profiles.** Persist repeated per-account environment overrides
in the same `config.toml` the client loads:

```toml
[run.profiles.work.env]
CLAUDE_CONFIG_DIR = "/home/me/.claude-work"
ANTHROPIC_BASE_URL = "https://api.anthropic.com"
```

```bash
ai-memory run --profile work claude --model opus
```

Profiles are env-only by design; executable paths, native arguments, `--yolo`,
and auto-wire choices stay explicit on the command line. The child inherits
the normal process environment, then profile values override it,
`--env-file` overrides the profile, and repeated `--env` entries win last.
The resolved values also drive native-session discovery and auto-wire, so all
three use the same account/config home. An unknown or invalid profile fails
before ai-memory takes a workstream lease or starts a harness. Profile names
use up to 64 ASCII letters, digits, `.`, `_`, or `-`; each profile may contain
up to 128 environment entries. Because values are literal and may include
credentials, protect `config.toml` like any other local secret-bearing config.

`--profile` is wrapper-owned only before the harness name. A later flag remains
native argv, which keeps OMP's own profile selector unambiguous:

```bash
ai-memory run --profile work omp --profile omp-work
```

**Multiple Claude accounts (e.g. Corporate and Personal).** Any harness name
starting with `claude` is accepted (`claude-corp`, `claude-personal`, ...)
and always selects the Claude harness — the exact spelling never changes
the agent kind, session store, or transcript import.
Combine that wildcard with `--executable` to launch the right account's
binary while keeping each account's managed workstream distinguishable in
your shell history:

```bash
# ~/bin/claude-corp and ~/bin/claude-personal are wrapper scripts (earlier on
# PATH than the bare `claude`) that each exec the real claude binary with
# that account's config/credentials directory.
ai-memory run claude-corp --executable claude-corp --model opus
ai-memory run claude-personal --executable claude-personal
```

Only the `--executable` value matters for which binary actually runs; the
positional name is free-form as long as it starts with `claude`, so pick
whatever reads clearly to you.

```bash
cd /path/to/project

ai-memory run claude
# quit Claude Code, then continue the same logical workstream in Codex
ai-memory run codex --yolo
# return to Claude Code later; ai-memory supplies Claude's native --resume
ai-memory run claude --model opus
# any `claude*` name is accepted (see "Multiple Claude accounts" above)
ai-memory run claude-corp
# Kimi Code installs `kimi`; `kimi-cli` is accepted as a launcher alias
ai-memory run kimi-cli
# Command Code uses `command-code` on Unix and `cmdc` on native Windows
ai-memory run command-code
# Kiro defaults to v2; select its incompatible v3 engine explicitly once
ai-memory run kiro
ai-memory run kiro --v3
# or omit the harness and continue the newest usable session automatically
ai-memory run
```

After an interactive managed session exits successfully, `ai-memory run` offers
the installed harnesses, a way to run the current harness again, and quit.
Choosing another harness keeps the same workstream selected, so its saved
context remains available to the next run. The prompt defaults to quit. It
appears only when stdin, stdout, and stderr are terminals, the managed launch
was a session, and `--executable` was not used; utility commands, failed or
interrupted exits, and non-interactive launches do not prompt. A switch or
re-run does not replay the previous harness's native arguments, which may be
specific to that CLI; wrapper settings such as `--workspace`, `--project`,
`--yolo` / `--true-yolo`, ai-jail controls, `--no-autowire`, `--env`, and
`--env-file` remain in effect. The initial `--fresh` choice applies only to the
first launch.

Everything after the harness name is native argv except the wrapper-owned exact
flags `--yolo`, `--fresh`, and `--force-unlock`. No `--` separator is needed,
and ai-memory does not maintain a second copy of each harness's option schema.
Other wrapper options come first:

Portable events, handoffs, and project briefs are injected as explicitly
delimited, untrusted historical data. Instruction-like text inside stored
content is evidence only: agents must not execute commands, expose secrets,
change permissions or policy, or use tools merely because that content asks.
Current system/developer/user instructions, the canonical project instruction
file, and the current checkout remain authoritative.

```text
ai-memory run [--workspace NAME] [--project NAME]
              [--workstream NAME | --new NAME] [--executable PATH]
              [--yolo] [--fresh] [--force-unlock] [--profile NAME]
              [--env KEY=VALUE]... [--env-file PATH] [--require-server]
              [claude|claude*|codex|opencode|opencode2|pi|crush|omp|kimi|command-code|kiro|grok|antigravity|copilot]
              [native arguments...]
```

The default is the most recently selected workstream for the current repository
and worktree, creating one named `default` on first use. `--new NAME` starts an
independent line of work; `--workstream NAME` returns to one. These are optional
branching controls, not harness-switch controls.

List the workstreams available to those selectors without launching a harness:

```bash
ai-memory workstreams [--workspace NAME] [--project NAME]
ai-memory workstreams --limit 50 --json
```

The list is scoped to the same `(workspace, project, repository, worktree)`
identity as `run`, puts the current selection first, then orders by recent
activity. Each row includes the linked harnesses and stable workstream id; the
response does not expose checkout paths, repository fingerprints, or native
session ids.

Names are chosen at `--new` time and can be corrected later:

```bash
ai-memory rename-workstream --from typo-nmae --to refactor-db
ai-memory rename-workstream --workstream-id 01a04092-… --to refactor-db
```

The two selectors are mutually exclusive; the id is the one `workstreams`
prints. The rename is metadata only. Names are unique per checkout, so a
destination another workstream already holds is refused rather than merged,
and the destination is validated exactly like a `--new` name. Because the
ledger, linked harnesses, and managed runs all key on the workstream id rather
than its name, nothing else moves — including which workstream a bare
`ai-memory run` resumes, and the listing order, both of which stay put because
a rename deliberately does not touch `selected_at` or `updated_at`. A run that
is already live keeps displaying the name it launched with until it exits.

## Do you need this?

Probably not at first — hooks alone already carry most continuity.

- **Skip it** when a handoff is all you want: you quit Claude Code
  mid-task, open Codex in the same directory, and the next session
  starts with "where you left off, what failed, what's open". That
  works with nothing but `install-hooks`; no `ai-memory run` involved.
- **Use it** when you want the harness's own native resume (`claude
  --resume` / the picker) to survive a harness SWITCH — the managed
  ledger records the visible event stream portably, so `ai-memory
  continue` can reopen the same workstream in a different agent with
  the exact tool-call history, not just a summary.

If you never switch harnesses mid-workstream, the default path is
simpler and loses you nothing.

## Project-first launcher

`ai-memory show` reverses the usual `cd` then `run` flow: choose a local
checkout, choose an installed managed harness, and launch from that checkout.

```bash
cd ~/Projects
ai-memory show

# Structured discovery only; never launches a harness.
ai-memory show --json
ai-memory show --json --no-scan
```

A successful managed prepare refreshes `<data_dir>/client-projects.json`, a
private client-local registry keyed by a normalized, credential-free server URL
and `(workspace, project)`. The server's `/api/v1/projects` response supplies
only project metadata; it never exposes or chooses a server-host checkout path.
This lets a laptop and desktop map the same remote homeserver project to
different local directories without syncing path precedence or conflicts.

By default the picker combines valid saved links with a bounded depth-1 scan of
the current directory. The scan recognizes common project markers, ignores
symlinks plus dependency/build directories, and resolves each candidate through
the same marker and repository rules as `run`. `--no-scan` uses saved links
only, while `--workspace NAME` filters both sources. Stale, retargeted, or
scope-mismatched links are skipped and a successful later `run` repairs the
entry. If the server is temporarily unavailable, saved links and scan results
remain selectable and a launch from the picker degrades exactly like any other
`run` (see [Degraded offline launches](#degraded-offline-launches)) rather
than failing closed; pass `--require-server` to refuse the launch instead.

Interactive mode begins with `+ New project`. The launcher accepts a portable
lowercase ASCII directory name, builds the marker, instruction routing, and
Agent Skills in a hidden staging directory, and renames it into place only when
all setup succeeds. `--yolo`, `--fresh`, and trailing native arguments apply to
the selected harness. Non-terminal callers must use `--json`; JSON cannot be
combined with launch arguments.

## Continuing from anywhere

Bare `ai-memory run` continues the current checkout, but its workstream lookup
is keyed by `(workspace, project, repo fingerprint, worktree fingerprint)`, so
the caller must already be in the project. `ai-memory continue` supplies the
missing step and needs no `cd`:

```bash
ai-memory continue
ai-memory continue --workspace work
```

The checkout is chosen entirely on the client, from the `linked_at` stamp that
every successful managed prepare writes to `client-projects.json`. The server
is never asked which directory to use — it does not expose host paths, and a
link can only be trusted after this host revalidates it.

Before launching, the newest link is rechecked twice: the recorded path must
still canonicalize to itself (rejecting a directory that moved or was replaced
by a symlink), and it must still resolve to the same `(workspace, project)`
(rejecting a checkout that would file this session's memory under a different
scope). A link failing either check is named on stderr and skipped, and the
next-newest link is tried. A corrupt `linked_at` timestamp is also reported and
never considered launchable. Falling through is never silent: the selected
project and path are always printed before the harness starts.

Once a checkout is selected, the launch is exactly bare `ai-memory run` in that
directory, including automatic harness selection. `continue` therefore accepts
`--workspace`, `--yolo`, and `--fresh`, but not native harness arguments or
`--executable`, whose meaning depends on a harness the user did not name.

## Picking a workstream

`ai-memory resume` presents all managed workstreams belonging to the **current
checkout**, then launches the selected named workstream there. Other repositories
and sibling Git worktrees are not included, even when they share a workspace,
project name, or remote URL. Running from a subdirectory of a checkout uses that
checkout's Git fingerprints and the nearest scope marker. To resume the newest
linked checkout from any directory, use `ai-memory continue` instead.

```bash
ai-memory resume
ai-memory resume --search auth
ai-memory resume --workspace work --limit 50
ai-memory resume --all        # every linked checkout, current one first
```

`--all` restores the cross-checkout view: it walks every checkout linked to
this server in the client-local registry (one per project), current checkout
first, with each checkout's own fingerprint-scoped, fully paginated query;
`--workspace` filters the registry before any request, and a checkout that can
no longer be resolved or queried is skipped with a note.
Listing reads the stable repository/worktree fingerprints without running
`git status`, so a working-tree scan or filesystem-monitor hook cannot hold up
the picker. The selected launch still captures its normal Git checkpoint.

Use Up/Down to move between workstreams and Left/Right to cycle the
launch harness for the highlighted row. Each row remembers its choice while you
navigate. `auto` is the initial choice and preserves bare `ai-memory run`'s
discovery of the newest usable session; the remaining choices are supported
harness executables detected in the host `PATH`. Enter launches the displayed
workstream/harness combination. Escape clears a non-empty search, then cancels
when the search is empty; Ctrl-C always cancels.

Just type to filter workstream names with a case-insensitive substring search;
there is no search mode or prefix key. Results update immediately, and every
printable character (including `j`, `k`, `q`, and `/`) is literal search text.
Backspace removes a character and Ctrl-U clears the query. Enter launches the
highlighted result directly. Arrow keys navigate and switch harnesses without
leaving the search.
An empty result is not selectable; edit or clear the search to recover.

There is no default cutoff: the CLI fetches successive bounded server pages,
and the picker scrolls with Up/Down, PageUp/PageDown, Home, and End. `--search`
sets the initial query. An explicit `--limit N` caps results **after** that
initial search; clear or change the search without the limit to browse all rows.
`--workspace` checks the current checkout's resolved workspace, rather than
switching to another checkout.

The current selection leads the picker, followed by recent
activity. Each row identifies its workspace/project, activity age, selected
launch harness, and already linked harnesses. Choosing a different harness is
how an existing workstream can be continued in another agent. `--yolo` and
`--fresh` are forwarded to the eventual managed launch.

The picker does not read the private `client-projects.json` registry or fall back
to another checkout when this one has no workstreams. It revalidates the canonical
path and resolved scope before launching the selection. The server
receives only the repository/worktree fingerprints required for the existing
checkout-local listing, never a host path. It needs an interactive terminal;
scripts can continue to use `ai-memory workstreams --json` after selecting a
checkout themselves. Pagination requires an updated server; if an older server
ignores the page offset, the CLI reports that an upgrade is needed instead of
looping forever or silently presenting a truncated list.

## Automatic harness selection

With no harness name, `ai-memory run` inspects checkout-local sessions for
Claude Code, Codex, OpenCode, Pi, Crush, Kimi Code, Command Code, and both Kiro
CLI engines. Before scanning OpenCode sessions, it probes the exact `opencode`
executable once and uses that resolved major for discovery, launch, auto-wire,
and transcript import. Executable lookup, the version probe, and the child launch
share one captured environment with `--env` / `--env-file` overlays, including
case-insensitive `PATH`/`Path` replacement on Windows. A missing, malformed,
timed-out, or unsupported OpenCode
probe skips only OpenCode; discovery continues with every other available
harness. For an empty workstream it resumes
the newest session automatically. For an established workstream, server state
takes precedence: ai-memory resumes the most recently linked harness that still
has a usable local session. It never chooses a newer but obsolete session from
another harness merely because that file has a later timestamp. Kiro's v2 and
v3 candidates share one server agent identity, but the selected native engine
flavor remains exact. OMP, Grok, Antigravity, and Copilot remain available explicitly
but are not in the automatic pool. OpenCode's resolved major contributes one
adapter to the pool, so shared V1/V2 storage never duplicates a candidate.

OpenCode 2 sessions run inside a shared background service, so its plugin
cannot see a managed run's environment the way in-process plugins do. Managed
`run opencode2` legs launch with the correct resume selectors, capture through
hooks, and import the beta transcript — all verified live — but the
ledger-delta acknowledgement (`context_delivered`) does not fire, because the
accept path requires the run id on the session-start hook. The failure mode is
redelivery, never loss: a later leg into another harness may receive the range
again. Until the beta offers per-invocation plugin context, cross-harness
continuity into opencode2 arrives through the ordinary handoff loop rather
than the ledger delta.

Bare mode accepts wrapper options but not harness-native arguments or
`--executable`, because their meaning depends on the selected harness. In a new
directory with no session in the automatic pool, it exits without creating a
workstream and suggests the explicit `ai-memory run <harness>` commands.

## First managed launch

An otherwise-empty workstream may adopt one of the requested harness's existing
native sessions. On an interactive launch, ai-memory inspects that harness's
store without modifying it and lists up to eight recent sessions whose recorded
working directory matches the current checkout. Choose one to resume it, press
Enter to accept the newest candidate, or choose `0` to start a new session.
Sessions from another checkout are never offered.

Adoption is only a bootstrap operation. Once any harness has linked a native
session or contributed portable message/tool/compaction history, the workstream
is established. If Claude established it and Codex has not joined it yet, for
example, `ai-memory run codex` creates a fresh Codex session and injects the
Claude workstream history. It does not inspect or select an older unrelated
Codex session. Returning to Codex later resumes the Codex session already linked
to that workstream.

Explicit native selectors always win. `--new NAME` always creates a fresh
native session for the new workstream. Scripted/noninteractive invocations and
launches without terminal input skip the chooser and start fresh. A launch that
exits before producing either a native session or portable history does not
consume the later adoption opportunity.

Before adding an ai-memory-owned resume selector, the launcher checks the exact
linked id in the harness's native store without modifying it. If the transcript
was deleted, cleared, or lost with a sandbox overlay, ai-memory starts a fresh
native session and repoints the same workstream when that session is observed.
An unreadable or malformed store is reported but is not mistaken for a missing
session. Use `ai-memory run --fresh <harness>` to deliberately skip the linked
session and the adoption chooser. `--fresh` cannot be combined with a native
resume, continue, session, or fork selector.

## What happens on each run

1. The host client resolves the normal workspace/project scope and a stable
   repository plus worktree fingerprint. If an identity-backed project still
   carries its legacy basename, this authorized write promotes the same UUID to
   its canonical path name and refreshes the wiki scope manifest before opening
   the lease; a post-commit manifest failure is returned as
   `manifest_warning` on either success or a later prepare error, retaining the
   first warning across busy retries and lease wait-out, printed once by the CLI,
   and left for startup repair. Pure request validation, including mutually
   exclusive workstream selectors, runs before promotion. It opens a 90-second
   renewable lease. One writer may own a workstream at a time, so
   two terminals cannot silently race its native-session pointers
   or delivery cursors.
2. Bare mode resolves the correct available harness. For an empty workstream,
   an explicit interactive adapter can offer matching local sessions for
   one-time adoption. Otherwise the adapter passes native arguments through in
   order and adds a create/resume selector only when the user did not supply one.
3. `AI_MEMORY_RUN_ID` marks lifecycle hooks as managed. SessionStart links the
   actual native session and injects only the portable events that session has
   not seen. Crush, which has no SessionStart hook, receives the same bounded
   packet through a temporary `options.global_context_paths` entry. Kimi Code
   fires SessionStart but discards its stdout, so the kimi adapter's
   SessionStart hook only captures the event — it neither fetches nor links.
   The UserPromptSubmit hook issues the `/handoff` GET with the native
   `session_id` in the query; the server links the session and renders the
   packet atomically, and Kimi Code injects the hook's stdout as a user
   message before the turn. A pending single-use handoff remains additive:
   it is placed before the managed packet, and both delivery claims commit
   together only after the full handoff/packet/brief response is assembled.
   Direct launches continue to use the same handoff path without a managed
   packet.
4. When the child exits, ai-memory reads the native transcript store without
   modifying it. A session named on the command line (or chosen before the
   spawn) is the one it reads. Otherwise a session linked during the run under
   its `AI_MEMORY_RUN_ID`, even the workstream's current one, is read when the
   native store holds it for this checkout; only without such a link does it
   look for the newest session in the checkout, which a concurrent launch
   there could own. Visible user/assistant messages, completed tool calls/results,
   compaction summaries, and a non-mutating Git checkpoint enter an append-only
   workstream ledger. Hidden reasoning and unsupported/private records are
   excluded and recorded as extraction-loss annotations. Each delivered
   workstream packet begins with a versioned origin marker. If Claude Code
   persists that packet and its `Read` tool returns it, the Claude transcript
   normalizer excludes the marked result instead of feeding delivered history
   into the ledger again. It also recognizes the pre-marker packet header for
   compatibility with existing native sessions.
5. Imports use deterministic event ids, incremental source cursors, immutable
   sanitized JSONL segments, and bounded batches. A retry cannot duplicate
   history. The native process's exit code is preserved.
6. Command Code, Kiro CLI and Antigravity CLI have no native session-end hook,
   so for them the run then finalizes the session itself (again after a
   resume), as `ai-memory finalize-session --reopen` would: summary, handoff
   and opt-in SessionEnd consolidation. It does this only for a session named
   on the command line, chosen before the spawn, or linked during the run,
   never for one it had to look for. If finalizing fails, it prints the exact
   `finalize-session` command to run instead.

The next harness receives a bounded recent delta because no agent context window
can safely absorb an unbounded transcript. The complete visible ledger remains
searchable from inside a managed agent process:

```bash
ai-memory workstream-search "scope resolver decision"
ai-memory workstream-search --limit 50 --json "failed migration"
```

`AI_MEMORY_WORKSTREAM_ID` supplies the id automatically inside the child. From
another shell, pass `--workstream-id <uuid>` explicitly. Search results preserve
the source harness, role, event sequence, and content. Historical tool activity
is labelled completed evidence and must never be replayed as a pending call.

Search and tail also return optional `source_record_id` and `metadata` fields.
The text CLI prints the source record and correlation metadata as escaped JSON;
`--json` preserves these fields for consumers. Older server responses without
them remain readable. Startup context packets keep their existing format and
omit these additional fields.

Source labels and string metadata values are scrubbed with the server's
configured sanitizer **before** the 512-byte UTF-8 cap. The metadata allowlist
matches the shipped adapters: string `tool`, `tool_call_id`, `tool_use_id`,
`parent_id`, `summary_type`, and `status`; boolean `is_error`; signed 32-bit
`exit_code`; and unsigned 64-bit `loss_count`. Unknown keys, nested objects,
arrays, and wrong scalar types are discarded. Empty metadata is omitted. These
labels describe untrusted historical evidence; they cannot grant authority,
choose a project, or prove a tool's success independently.

The same scrub and allowlist protect legacy SQLite rows during search/tail,
without rewriting them. Malformed metadata or legacy dumps over 16 KiB return
no metadata. Existing raw segments are not rewritten by a read. The ledger's
FTS query, ranking, ordering, and limits remain unchanged; provenance is fetched
in the existing event query, without additional per-result SQL reads.

`event_id` remains the deduplication key. Native adapters derive it from the
original source before provenance redaction; truncating or redacting two source
labels therefore cannot merge distinct event ids. Repeating an event is a
no-op when its stable identity (`agent`, `native_session_id`, `kind`) matches.
The first indexed content, role, timestamp and provenance remain unchanged,
even if a later upload differs or the sanitizer's patterns have changed. This
also permits replay of legacy rows above today's write bounds; read-time
provenance sanitization still applies. Changing the stable identity is rejected
and the SQL batch rolls back. A rejected immutable raw segment is not indexed;
a corrected retry can import the valid batch without advancing history for the
rejected one. Finished runs retain the existing no-op behavior for late events,
including new event ids; their history and latest sequence do not advance.

Client event ids beginning with `managed-run:` are refused before raw storage,
including on retries of a finished run. This namespace belongs to the server's
checkpoint and extraction-loss events, which it appends when completing an
active run.

## Degraded offline launches

When the ai-memory server is unreachable — a remote homelab down for
maintenance, a VPN that is not up — `ai-memory run` does not abort. It probes
the server first (any HTTP answer counts as reachable, so an older build
without `/healthz` still passes), prints one loud warning naming the server
URL and what the degraded run means, and launches the harness anyway:

- **Unavailable during the run**: no workstream lease or cross-harness context
  packet, immediate transcript import, or handoff delivery. Nothing that needs
  the server runs: no prepare, lease, link, heartbeat, context fetch, status
  check, finish, or import.
- **Recovered afterward**: recovery requires both the `ai-memory run` client
  and server to be upgraded to the release that provides `ai-memory recover`
  and modern per-item hook acknowledgements. The harness's ai-memory lifecycle
  hooks (if installed) spool events locally. After the child exits, the launcher
  discovers its native session and appends only bounded metadata (never
  transcript content or secrets) to an owner-only local recovery journal. Run
  `ai-memory recover` when the server returns: under the spool's exclusive
  drain lock it first quarantines entries correlated to each journaled session,
  drains unrelated entries, then re-exports the exact native transcript and
  replays session start, content, and session end through the existing
  sanitized `/hook/batch` backfill path. A successful replay deletes the
  quarantine; failure restores it. Antigravity's native store has no supported
  transcript exporter, so its degraded launch is still journaled explicitly as
  spool-only and recovery never invokes its exporter. Its correlated entries
  are quarantined and the journal clears as `recovered-via-spool` only after
  their durable delivery was confirmed.
  An empty spool, a correlated prior drop, or a launch with no hook evidence
  stays journaled with a manual-repair status. Stable recovery ingest keys retain the
  exact session, agent, event kind, and sanitized event identity until
  session/project deletion; an ordinary writer cannot preclaim one for another
  event. Ordinary hook keys still expire after 30 days. An existing MCP
  registration still degrades to no-recall during the outage rather than
  blocking the session.
- **Sessions**: because no lease exists there is no mutual exclusion against
  another launcher in the same checkout, so a degraded launch never adopts or
  resumes a session implicitly. An explicit native session selector still
  resumes the named session; before such a resume the launcher persists the
  native adapter's exact pre-launch cursor, and recovery exports only records
  after it. If that cursor cannot be proven, the launch remains usable but is
  not journaled for automatic replay. Otherwise the launcher snapshots
  checkout-local candidates before spawn and journals only when exactly one new
  session appears. Ambiguous concurrent launches are not journaled and print
  the manual `backfill --force --session <id>` recovery action.
- **Auto-wire**: the first-launch hook install still happens (it is local and
  idempotent, and capture must spool offline), but no MCP entry is registered
  for the now-unreachable server and no completion sentinel is written, so the
  next online launch finishes the wiring. Existing registrations are never
  touched.
- **Attribution**: the child is launched without `AI_MEMORY_RUN_ID` /
  `AI_MEMORY_WORKSTREAM_ID`, so its SessionEnd hook cannot attribute the
  session to a server run that never happened.
- **Exit code**: the child's own exit code is returned; the launch itself is
  not an error.

When the child exits, the launcher prints that the run was not recorded on the
server, how many hook events remain spooled locally, and that the run was
journaled for `ai-memory recover`. The journal is capped at 256 entries;
once full it refuses new entries with an actionable warning instead of evicting
unrecovered work. It is written atomically as owner-only local state; Unix lock opens use
`O_NOFOLLOW` and the atomic replacement rechecks the destination immediately
before rename. Windows applies an owner-only ACL and rejects observed symlinks,
but reparse-point race hardening still requires the PR `windows` CI lane before
merge. Spooled events remain separately
bounded: they are dropped after the configured number of failed drain passes
(8 by default), 7 days of age, or the 10,000-file spool cap. For a planned
outage set `AI_MEMORY_HOOK_SPOOL_MAX_ATTEMPTS=0` to disable only the
attempt-count drops (see `docs/install.md`).

If the server dies *during* a run — after the lease was acquired — the exit
code is still preserved. The journal records the original run id, native
session locator, absolute store path, and exit code; repository branch and path
state are not persisted. Recovery retries validate the exact ordered semantic
transcript interval and native identity, and omit a regenerated checkpoint, so
repository changes during downtime are not claimed as the failed run's state and
a successful finish cannot create a second checkpoint event.
`ai-memory recover` uses the owner-authorized `/recover/finish` route to resume
an expired original run only when no newer run superseded it; ordering is the
explicit `(started_at, SQLite insertion order)` tuple, so timestamp ties cannot
revive an older run. Another operator, a superseded run, and any finished-run
payload that is not an exact replay of already indexed events/cursor/exit state
remain refused.
Successful entries are removed, while failed entries remain with a bounded
sanitized reason. Each journal request and the whole journal pass have time
budgets. A spool pass that drops anything is reported incomplete and keeps all
journal entries; correlated entries are quarantined before that pass and can
only be deleted after a later clean pass and exact transcript replay. If the
server is still unreachable, `recover` exits successfully
and prints the remaining spool/journal counts and startup guidance.

To restore the strict behavior (fail with the usual "could not reach …"
diagnosis and never start the agent), pass `--require-server`, set
`run.require_server = true` in `config.toml`, or export
`AI_MEMORY_RUN_REQUIRE_SERVER=true`.

## Native identity privacy

New managed bindings accept the exact original native identity only when it is
nonempty, at most 512 UTF-8 bytes, free of controls and unsafe invisible
formatting, and unchanged by the privacy scrubber. IDs are never trimmed,
normalized, truncated, or replaced with a redaction placeholder to make a
binding succeed. Existing vendor IDs and generated fresh-session UUIDs remain
supported. Each adapter retains its own path and selector restrictions.

The launcher validates prepared and linked IDs from older servers before resume
or native-store use, and validates finish identities before serialization.
Configured privacy rules also apply to post-launch status and discovered IDs
before transcript export and finish transport.
HTTP ingress applies the configured privacy rules before segment writes; the
writer also validates identities before link/import SQL, after the existing
owner and current Write checks on finish. A refused hook auto-link still leaves
regular sanitized shared observation capture and legacy UUID/v5 session routing
intact. A run without a native ID keeps its existing unbound checkpoint behavior.

`workstream-search --json` keeps its existing String field: an invalid historical
`native_session_id` is returned as `""` (UNKNOWN), which cannot be used for a new
managed binding. Valid identities retain their exact bytes. The stored value,
event content, rows, source cursor, and shared history are not rewritten. The
CLI repeats the privacy projection for older-server responses.

This boundary establishes identity representation and privacy only. It does not
prove a native file exists, authenticate a source ID, correlate a run with an
event, or establish a tool outcome. SQL refusal remains transactional; this
change does not add an atomic disk/SQL seal or change lease/retry policy.

## Native adapter behavior

| Harness | Fresh native session | Returning native session | Read-only source |
|---|---|---|---|
| Claude Code | generated `--session-id` | `--resume <id>` | `~/.claude/projects/**/*.jsonl` |
| Codex | native default creation | `resume <id>` | `~/.codex/sessions/**/rollout-*.jsonl` |
| OpenCode V1 | native default creation | `--session <id>` | `~/.local/share/opencode/opencode.db` opened read-only; selected when the exact launch executable reports major 1 |
| OpenCode V2 | native default creation | `--session <id>` | same `opencode.db` filename, using the `session_v2`/`session_message` tables; selected when the exact launch executable reports major 2 |
| Pi | generated `--session-id` | `--session <id>` | `~/.pi/agent/sessions/**/*.jsonl` |
| Crush | native default creation | `--session <id>` | `<data dir>/crush.db` opened read-only: `options.data_directory` from Crush's JSON configs, else the closest `.crush` up to the git worktree root, else `<cwd>/.crush` |
| Kimi Code | native default creation | `--session <id>` | `$KIMI_CODE_HOME/sessions/*/*/agents/main/wire.jsonl` |
| Command Code | native default creation | `--session <uuid>` | `~/.commandcode/projects/*/<uuid>.jsonl` |
| Kiro CLI v2 | native default creation | `--resume-id <uuid>` | `$KIRO_HOME/sessions/cli/<uuid>.jsonl` (+ sibling `<uuid>.json` metadata) |
| Kiro CLI v3 | native default creation with `--v3` | `--v3 --resume-id <sess_uuid>` | `$KIRO_HOME/sessions/<checkout-bucket>/<sess_uuid>/messages.jsonl` (+ sibling `session.json` metadata) |
| OMP | native default creation | `--resume=<id>` | `<agent dir>/sessions/**/*.jsonl`, or the XDG session directory described below |
| Grok Build CLI | generated `--session-id` | `--resume <id>` | `$GROK_HOME/sessions/*/*/chat_history.jsonl` |
| Antigravity CLI | native default creation | `--conversation <id>` | `~/.gemini/antigravity-cli/conversations/<id>.db` metadata; user prompts only from `~/.gemini/antigravity-cli/history.jsonl` (assistant and tool steps come from lifecycle-hook capture) |
| GitHub Copilot CLI | generated `--session-id` | `--resume=<id>` | `$COPILOT_HOME/session-state/<uuid>/events.jsonl` (`COPILOT_HOME` defaults to `~/.copilot`) |

OMP's agent directory is `~/.omp/agent` for the default profile and
`~/.omp/profiles/<name>/agent` for a named profile. `PI_CONFIG_DIR` changes the
`.omp` root relative to the user's home. The default profile can also use
`PI_CODING_AGENT_DIR`. On Linux and macOS, sessions move to
`$XDG_DATA_HOME/omp/sessions`, or `$XDG_DATA_HOME/omp/profiles/<name>/sessions`
for a named profile, when that OMP directory exists and the agent directory
has not been relocated.

Command Code v3 transcripts are self-describing and append-only. The adapter
requires the UUID filename, header id, and canonical header `cwd` to agree
before discovery or resume. Its 1.14.1 allowlist was checked against both the
integrity-matched published package and a sanitized live fixture. It imports
visible messages, compactions, and branch summaries, retains `parentId` as
branch provenance, and excludes hidden thinking, images, harness-injected
messages, provider/model metadata, custom/Mod records, and every sidecar. An
unknown transcript version fails closed until audited.
The default executable is `command-code` on Unix and `cmdc` on native Windows;
`commandcode`, `cmdc`, and `cmd` are accepted launcher aliases. Exact user
`--session`, `--resume`, `--continue`, and fork choices remain authoritative.
The experimental unsandboxed Mod API is not used.

An explicit native selector such as Claude's `--resume`, OpenCode's `--session`,
Codex's `resume`, Antigravity's `--conversation` / `--continue`, or Copilot's
`--resume` / `--continue` / `--session-id` / `--connect` wins.
ai-memory links the selected native session and resets an unrelated adapter
cursor rather than assuming it belongs to the old session.

When the linked Claude session is a Claude Code background session that is
still running in the daemon, Claude refuses `--resume <id>` when another flag
comes with it (seen with `--model`, `--effort` and
`--dangerously-skip-permissions` on Claude Code 2.1.287; a bare `--resume`
attaches) and points at `claude attach <id>`. Only when the
transcript shows the session ran in the background (`sessionKind: "bg"`)
does ai-memory ask `claude agents --json --cwd <checkout>`. When that lists the
session as a live background one in this checkout, it launches
`claude attach <id>` with the listing's short id instead. Native arguments are
not passed to the attach client. In the Claude Code 2.1.288 lifecycle validation,
no new `SessionStart` was observed from the attach client; the background
worker's hooks carried the background session id. Delivery of a pending
workstream context packet through attach remains unverified: that validation
did not exercise a pending packet. The launcher reports this limitation and
suppresses the missing-acknowledgement warning for attach. (#1052)

When the attach client detaches or exits early, the managed run finishes with
the client's exit status and imports the linked transcript available at finish.
It neither stops the daemon nor waits for the background turn to complete;
events written after that import remain outside it. In the same validation,
Ctrl-Z detached with exit 0, and terminating only the attach client produced
exit 1 while the daemon continued and emitted `Stop` after the run finished.

A failed, unavailable or unexpected `claude agents` listing keeps the native
resume and its original flags. That fallback can still encounter Claude's
refusal to resume a live background session with flags. The lifecycle test
injected an exit-127 listing and an incompatible JSON object, then exercised
the real resume and backend; an older Claude binary without `agents` was not
tested. These observations are recorded in the
[PR #1067 lifecycle validation](https://github.com/akitaonrails/ai-memory/pull/1067#issuecomment-5962340148).

Claude Code background sessions run inside the Claude Code daemon, separately
from the attach client. Their hooks use the daemon worker's inherited
environment: the 2.1.288 validation observed an `AI_MEMORY_RUN_ID` from an
earlier launch, different from the current attach runs. A daemon hook therefore
does not by itself identify the current attaching run. When a managed Claude
session attaches to one (`/resume` on a session shown as
"running in the background"), the conversation goes on in the background
session's transcript. At the end of the run, ai-memory looks for a transcript
written during the run whose `sessionKind: "bg"` records name the run's own
session as the attached client and this checkout as `cwd`, and finishes the run
on that background session, so the next launch resumes it instead of the empty
foreground session. A background session attached by another launch is never
taken. (#1050)

Crush has no hooks to link its session: a fresh Crush launch claims the one top-level session
created while it ran (its title and sub-agent sessions do not count), and
imports nothing, with a warning, when another launch on the same store created
one too; resume that session with `--session <id>` to link it. `--continue`
claims the one session it touched on the same terms. When the data directory
lies outside the project, as a global `data_directory` does, other projects'
sessions share it and Crush records no directory per session, so a run claims
only a session that edited a file in this project (its sub-agents' edits
count), and only when it is the one; otherwise it imports nothing, with the
same warning. The store's real location decides, so a `.crush` symlinked to a
shared directory is shared. Another project's session that edited a file here
still counts as this project's.
Pi and OMP `--session-dir` values and Crush `--data-dir` values are passed
through unchanged and used as the read-only import root. Without `--data-dir`,
ai-memory finds Crush's data directory as Crush does, except that it does not
run a `crushrc` to read one set only there; pass `--data-dir` in that case. Native store
environment overrides are also honored:
`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME`,
`PI_CODING_AGENT_SESSION_DIR`, `PI_CODING_AGENT_DIR`, `KIMI_CODE_HOME`,
`KIRO_HOME`, and `GROK_HOME`, plus OMP's `PI_CONFIG_DIR`.
OMP profile selection follows a leading `--profile`, then `OMP_PROFILE`, then
`PI_PROFILE`. A named profile ignores `PI_CODING_AGENT_DIR`. A `--profile`
later in the command line is left to OMP; use `OMP_PROFILE` or
`--env OMP_PROFILE=<name>` so ai-memory can resolve the same profile.

Blank directory overrides (empty or whitespace-only values) count as unset.
Session import, hooks and MCP use their fallback paths, and `ai-memory run`
removes those values from the child environment. Profile variables follow a
different rule: an empty `OMP_PROFILE` selects the default profile and still
takes precedence over `PI_PROFILE`.
Export these in the environment `ai-memory run` itself sees, or pass them with
`--env` (below), not only inside a harness wrapper script. `ai-memory run`
resolves the native session directory, and auto-wires hooks and MCP, from that
environment; if the harness writes its transcript under a custom
`CLAUDE_CONFIG_DIR` that `ai-memory run` cannot see, the two disagree and the
native transcript import fails. When you use per-account
config directories, set the variable before invoking `ai-memory run` (or in the
same wrapper that also runs it), so hook installation and native-session
resolution agree.

A repeatable `ai-memory run --env KEY=VALUE <harness>` (and `--env-file
<path>`, one `KEY=VALUE` per line, blank lines and `#` comments skipped) is
the first-class alternative to the `env KEY=VAL harness` wrapper-alias
pattern above: it is a wrapper-owned flag, so it must precede the harness
name, and the resolved environment reaches the spawned harness process,
`ai-memory run`'s own native-session resolution, *and* first-launch auto-wire.
Hooks, MCP and transcript import then follow the same config home, which is the
`CLAUDE_CONFIG_DIR`-agreement requirement described above, without exporting the
variable into the invoking shell first. A later `--env` overrides a same-key
`--env-file` entry; neither expands nor interprets the value, so let the shell
expand `$HOME` on the command line and write absolute paths in an
`--env-file`. Manual `install-hooks` / `install-mcp` do not take `--env`; they
read their own environment.

Automatic harness selection (bare `run`, `continue` and `resume`) scans the
store the launch resolves from that same environment, so a checkout whose
sessions live under a custom `CLAUDE_CONFIG_DIR` is found when the variable is
set. Whenever the client links a session, at launch or when the run finishes,
it also records that session's store in the client-local `client-projects.json`.
If a later launch cannot find the linked session in the store it resolves and
the recorded store is a different directory, the launch stops with an error
naming both directories instead of starting fresh and repointing the workstream
away from a session that still exists. Relaunch with the same variable (or
store flag) to resume it, or pass `--fresh` to start a new session. Sessions linked before this record
existed, and a session missing from its own recorded store, still start fresh
as before.

The Pi-family adapter
also recognizes a complete `.jsonl.<nonce>.tmp` atomic-write file when a native
process exits before renaming it; incomplete final JSONL records are never
imported. Help, version, and known utility subcommands pass through without
session flags. Claude/Pi/OMP print mode, Codex `exec`, OpenCode/Crush `run`,
redirected input, and other noninteractive launches never open the adoption
chooser.

`ai-memory run --yolo <harness>` and `ai-memory run <harness> --yolo` both use
the harness's native dangerous mode. The translation is Claude Code
`--dangerously-skip-permissions`, Codex
`--dangerously-bypass-approvals-and-sandbox`, OpenCode `--auto`, Pi `--approve`,
Crush `--yolo`, Kimi Code `--yolo`, Command Code `--yolo`, Kiro CLI v2
`--trust-all-tools`, Grok Build CLI `--yolo` (equivalent to its
`--always-approve` option), Antigravity CLI
`--dangerously-skip-permissions`, and GitHub Copilot CLI `--yolo` (equivalent
to its `--allow-all` option). Kiro v3 replaced the trust-all flag with
`permissions.yaml`, so ai-memory prints a notice and adds no unverified flag.
OMP currently needs no added flag. ai-memory does not add a duplicate when the
translated native flag is already present.

Managed support is intentionally narrower than the general integration matrix.
Gemini CLI, Devin CLI, Cursor, and other agents may
have MCP or lifecycle-hook support without native managed resume. Contributors
adding another managed harness must follow the [managed-harness contribution
protocol](managed-harness-contributions.md), including read-only extraction,
pre-turn context delivery, migration invariants, deterministic tests, and an
opt-in real-harness acceptance pass.

### Codex shared-daemon recovery (#987)

Recent Codex releases run sessions through a shared background app-server
daemon (`codex agents` lists it). The daemon keeps the environment it started
with, and the lifecycle hooks it launches inherit that environment — including
the `AI_MEMORY_RUN_ID` of whichever managed run auto-started it. When a later
managed Codex SessionStart reports that finished id, ai-memory ignores it as
authority and searches only the request's already-resolved repository, exact
checkout cwd, and operator bucket. The server adopts a replacement only when
exactly one live, undelivered Codex run is waiting there, and it selects plus
links that run in one writer transaction. Multiple candidates, an active id
whose scope, checkout, or owner does not match, and a run already linked by
another session all fail closed.

Ordinary managed Codex launches therefore work with the shared daemon. For
diagnosis, or when an intentionally concurrent pair of named workstreams makes
recovery ambiguous, native arguments after the harness are still forwarded:

```bash
ai-memory run codex --no-daemon
```

`--no-daemon` makes that one session run without the shared background server
even if one is already running; it is available on Codex's interactive and
`resume` commands (checked on Codex 0.156). It remains a useful isolation
switch, not a requirement for normal workstream continuity.

## Installation and recovery

Managed runs need current ai-memory lifecycle hooks so SessionStart can receive
the portable delta. Refresh them after upgrading:

```bash
ai-memory install-hooks --agent claude-code --apply
ai-memory install-hooks --agent codex --apply
ai-memory install-hooks --agent opencode --apply
ai-memory install-hooks --agent pi --apply
ai-memory install-hooks --agent omp --apply
ai-memory install-hooks --agent kimi-code --apply
ai-memory install-hooks --agent kiro-cli --apply
```

Kimi Code hooks installed as native `ai-memory hook` commands automatically
pick up the current delivery behavior when the binary is upgraded. A
script-fallback installation must rerun the Kimi Code `install-hooks` command
after upgrading so its staged scripts are refreshed. Current hooks deliver
handoffs at `UserPromptSubmit`; Kimi discards `SessionStart` stdout.

Known Kimi Code adapter limitations: subagent transcripts
(`agents/<id>/wire.jsonl` other than `main`) are not imported in v1 and are
recorded as an extraction-loss annotation; the session bucket directory name
is a one-way hash of the working directory, so discovery always reads
`state.json`'s current `cwd` field or legacy `workDir` alias and never parses
the bucket name. Conflicting aliases or a persisted id that disagrees with the
session directory are rejected. Event ids derive
from the SHA-256 of the raw wire.jsonl line, so two byte-identical lines —
only possible with identical content in the same millisecond, because Kimi
Code stamps each record with `time` — collapse into a single ledger event.
The incremental cursor stores both the complete-record byte offset and a
SHA-256 of that imported prefix. Normal appends resume at the saved offset;
if Kimi rewrites `wire.jsonl` in place, ai-memory resets to the beginning and
replays the file, with stable event ids deduplicating records already in the
workstream.
Legacy sessions that keep `wire.jsonl` directly in the session directory
(the pre-`agents/` layout the kimi session-store still reads through its
stat fallback) are neither discovered nor imported in v1. The native
contract was reverified against Kimi Code v0.34.0. The managed launcher accepts
`kimi`, `kimi-code`, and `kimi-cli`; all three resolve the installed `kimi`
executable.

Kiro's version-aware adapter was live-tested with authenticated Kiro CLI
2.16.2 in both engines. V2 uses UUID session IDs and the flat
`$KIRO_HOME/sessions/cli/<uuid>.json` plus `<uuid>.jsonl` store with v1
`Prompt`, `AssistantMessage`, and `ToolResults` events. V3 uses incompatible
`sess_<uuid>` IDs and nested
`$KIRO_HOME/sessions/<checkout-bucket>/<sess_uuid>/session.json` plus
`messages.jsonl`; accepted metadata is limited to `schemaVersion = 1.0.0`,
`dataModelVersion = 1`, an exact directory/id match, and a `workspacePaths`
entry resolving to the current checkout. The v3 visible-event allowlist is
user text, assistant `Say` output, tool calls, and tool results. Session
bookkeeping, hook records, usage summaries, turn boundaries, private assistant
operations, malformed records, and unknown schema versions are not imported.

The engines can never cross-resume: exact store metadata is checked before a
linked `--resume-id` is injected, and the incompatible engine flavor is also
stored in the opaque incremental cursor. Explicit `--v3`, v3-only `--mode`,
or `--agent-engine v3` selects v3; explicit `--agent-engine v2` selects v2; an
unknown engine value remains passthrough instead of being guessed. Once a v3
session is linked, a later plain `ai-memory run kiro` recovers that engine
transparently. Kiro CLI 2.16.2 wrote v3 sessions below the default
`~/.kiro/sessions` even when `KIRO_HOME` redirected other state, so ai-memory
checks the configured v3 root first and that default root as a compatibility
fallback. If a linked session exists only in the fallback, ai-memory removes
`KIRO_HOME` for that one resume so Kiro can find the session; Kiro consequently
uses its default-home v3 settings/hooks for that process, and first-launch
auto-wire wires that default home rather than `KIRO_HOME`. Fresh launches and
versions that store the session below the configured root keep `KIRO_HOME`
unchanged. Every candidate still needs exact id, schema, and checkout metadata.
The v2 `--yolo` translation is `--trust-all-tools`; an explicit narrower
`--trust-tools` choice is never widened. V3 documents no equivalent CLI flag.
See Kiro's current
[session management](https://kiro.dev/docs/cli/chat/session-management/) and
[v3 compatibility](https://kiro.dev/docs/cli/v3/) references.

Grok needs no ai-memory hook installation for managed delivery either. Grok
ignores `SessionStart` stdout and its `UserPromptSubmit` hook is passive, so
the launcher fetches the bounded context packet from the server and passes it
through Grok's native `--rules` flag, which appends the text to that session's
system prompt. Delivery is acknowledged only after the child spawns. Because
`--rules` is single-use in Grok's argument parser, a natively supplied
`--rules`/`--append-system-prompt` wins and the packet stays undelivered until
a later managed run can accept it. Grok can rewrite `chat_history.jsonl` in
place on rewind, so the import cursor stores a prefix hash and replays from
the beginning when it no longer matches, with content-hash event ids
deduplicating records already in the workstream. Sibling session files
(`events.jsonl`, `updates.jsonl`, `rewind_points.jsonl`) carry harness
internals and are never read as transcripts; discovery reads
`summary.json`'s `info.cwd` and never parses the URL-encoded bucket name. The
managed launcher accepts `grok` and `grok-build`. The native contract was
verified against Grok Build CLI v0.2.111.

Antigravity keeps one SQLite database per conversation at
`~/.gemini/antigravity-cli/conversations/<conversation-id>.db`, so the id is the
file name and no scan is needed to locate one. The workspace a conversation was
opened on comes from `trajectory_metadata_blob`, a protobuf message whose first
field holds a nested message whose first field is the workspace `file://` URI;
only those two fields are read. A database that does not carry them — an older
or newer `agy` — is skipped rather than failing the listing. Note the recorded
workspace is the directory `agy` was launched from, not a checkout root, so a
conversation started one level up is not offered inside a subdirectory.

`agy` accepts no caller-chosen id for a new conversation, so a fresh launch
injects no selector and the id is linked by the hooks or discovered after exit; a
linked resume passes `--conversation <id>`. `--continue` / `-c` is treated as an
explicit user choice and is never overridden. `--yolo` maps to
`--dangerously-skip-permissions`. Step payloads are undocumented, unversioned
protobuf blobs, so ai-memory does not decode conversation text: the visible-event
ledger for this harness comes from lifecycle-hook capture, and transcript export
fails with a message saying so. The managed launcher accepts `antigravity`,
`antigravity-cli`, and `agy`. The native contract was verified against
Antigravity CLI v1.1.7. Antigravity is not part of the no-argument
auto-detection set; name it explicitly.

GitHub Copilot CLI keeps each session in
`$COPILOT_HOME/session-state/<session-id>/` (`COPILOT_HOME` defaults to
`~/.copilot`). Copilot officially accepts a caller-chosen UUID through
`--session-id`, so a fresh launch generates one; a linked resume passes
`--resume=<id>`, in the `=` form because `--resume` takes an optional value and
would otherwise consume the next argument. A user-supplied `--resume`, `-r`,
`--continue`, `--session-id`, or `--connect` is an explicit choice and is never
overridden; a resume by session name or id prefix is resolved by Copilot and
linked after the fact. Discovery reads the first `events.jsonl` record,
`session.start`, and requires its `sessionId` to match the UUID directory name
and its `context.cwd` to match the checkout. Export imports the typed
`user.message` text (not the `transformedContent` copy carrying injected
datetime and context), visible `assistant.message` text and `toolRequests`,
the model-visible output of each `tool.execution_complete` (or the error
message of a failed call), and successful `session.compaction_complete`
summaries. System prompts, hidden reasoning (`reasoningText`,
`reasoningOpaque`, `reasoningBlocks`), model message snapshots, hook output
(where ai-memory's own startup packet is recorded), permission prompts, and
session telemetry are excluded. The journal stayed append-only across resume
and compaction in testing, but that is not documented, so the cursor stores a
prefix hash and replays with content-hash event ids if the file is ever
rewritten. Context is delivered by Copilot's existing SessionStart hook through
its top-level `additionalContext`. `--yolo` maps to Copilot's native `--yolo`;
an existing `--yolo` or `--allow-all` is not duplicated. The managed launcher
accepts `copilot` and `copilot-cli`: `run` launches only command-line
harnesses, so the bare name means the CLI here, while `install-mcp --client
copilot` keeps meaning VS Code Copilot. The native contract was verified
against GitHub Copilot CLI 1.0.92. Copilot is not part of the no-argument
auto-detection set; name it explicitly.

Crush needs no ai-memory hook installation for managed mode. The launcher reads
its one-time context from the server, copies the global Crush JSON the launch
would read (`$CRUSH_GLOBAL_CONFIG/crush.json`, else
`$XDG_CONFIG_HOME/crush/crush.json`, else `~/.config/crush/crush.json`, with
`--env` entries first and a blank value counting as unset) into a private
temporary directory, appends an ephemeral context path, and points the child at
that directory with `CRUSH_GLOBAL_CONFIG`. When that JSON lists no
`global_context_paths`, the launcher first adds the `CRUSH.md` and `AGENTS.md`
Crush would have loaded by default, so the packet does not replace them. A
global `crushrc` next to that JSON is carried over by a generated `crushrc` in
the temporary directory that sources it from its own directory, where Crush
runs it. Delivery is acknowledged
only after the child starts, so a spawn failure cannot lose the packet. The
original config is not modified. ai-memory opens the project database read-only;
the launched Crush process continues its normal native session writes.

The Linux/macOS Docker shell wrapper cannot inspect host projects or execute a
host agent from inside its helper container. For `run`, `show`, `continue`,
`resume`, and `workstreams`, it downloads the matching native release into
`${XDG_DATA_HOME:-~/.local/share}/ai-memory/native-runner`, verifies the
published SHA-256 checksum, and executes that host client. The release's
`hooks/` bundle is kept beside it so auto-wire can stage hook scripts on a host
where `install-hooks` never ran. The client lives with the host's ai-memory data
rather than under `~/.cache` because auto-wired hook configuration runs it
directly: a cache flush must not break capture. A client downloaded by an older
wrapper stays in `~/.cache/ai-memory/native-runner`, and hooks auto-wired from it
keep that path until a newer client version auto-wires again. To move them now,
re-run `ai-memory install-hooks --agent <agent> --apply`, then delete the old
directory. Set `AI_MEMORY_NATIVE_BIN=/path/to/ai-memory` to use a
specific native build. Native package, release, and source installs need no
shim. On native Windows, use the published `ai-memory.exe` or a source build.

The wrapper intercepts all five commands before Docker and preserves the host
`PATH`, `AI_MEMORY_SERVER_URL`, and authentication environment. The native client's
startup log shows `server_url` as well as its local config paths; `data_dir` and
`bind` describe local defaults and do not override a configured remote server.
If logs show
`data_dir=/data` followed by `starting managed ... No such file or directory`,
the installed wrapper is stale and sent the command into the helper container.
Run `ai-memory upgrade` on the client machine. A remote/homelab server must be
upgraded separately.

On a normal exit, ai-memory imports the transcript and closes the lease before
returning. Handled setup, launch, or import failures cancel the lease
immediately. A new launch retries an active-workstream conflict briefly so a
previous launcher can finish; if another harness is genuinely still running,
the conflict remains and concurrent writers are still rejected.

### Lease recovery

A launcher that dies without releasing its lease — killed, its terminal
closed, or a sandbox such as ai-jail torn down — leaves the workstream held
until that lease lapses. An interactive relaunch (stdin and stderr are
terminals) no longer fails on that: the conflict reports the lease's expiry, so
ai-memory says who holds it and waits for it to lapse (at most one lease,
~90 seconds; `Ctrl+C` aborts), then starts normally. If the holder renews the
lease while you wait, it is a launcher that is still running, and you get an
error instead — stop it, or pass `--new <name>` for a separate workstream. The
server's busy check stays the only arbiter: the waiting launcher never forces
another run off. Non-interactive launches (scripts, hooks, CI) keep the short
retry window and fail fast rather than hanging. Terminal
interrupts continue to reach the child while the parent stays alive to finish
or cancel the run.

When you know the prior launcher is gone and do not want to wait for the lease,
force-expire it explicitly:

```bash
ai-memory run --force-unlock codex
# The exact wrapper flag is also accepted after the harness name.
ai-memory run codex --force-unlock
```

The replacement is atomic and limited to the same durable authenticated
operator; in single-user or otherwise unattributed operation, both runs must be
unattributed. A different operator's active run is still refused. The command
expires the managed lease only — it does not signal or kill a native process.
If the previous launcher is actually alive, its later heartbeats and finish are
rejected, and its final transcript tail may not be imported. Use
`--force-unlock` only after verifying that launcher has stopped. Older servers
do not honor the request and return a refusal, so upgrade the server as well as
the client before relying on this recovery path.

Before the child starts, `Ctrl+C` at the native-session chooser cancels the
acquired run and exits without requiring Enter or adopting the selected session.
The launcher waits for the server's cancellation response. A request error is
reported and leaves the lease to expire within its normal 90-second window;
a server that accepts the request but never responds can still keep the launcher
waiting. Heartbeats have stopped, so the lease itself still expires.

While the harness or native-session selector is open, a temporary server outage
produces one short notice instead of printing every failed heartbeat. The
launcher keeps probing every 30 seconds with a 10-second request timeout so the
90-second lease stays safe across ordinary server restarts. Repeated failures
are quiet; when the server responds again, one recovery notice confirms that
heartbeats resumed. The native harness remains usable throughout the outage.
If the outage exceeds one lease window, the original launcher may renew its run
only while no newer launcher has claimed the workstream. A replacement prepare,
cancel, finish, or destructive operation remains terminal for the old run.

If the client is terminated without cleanup, such as with `kill -9`, its lease
expires within 90 seconds. A later managed run starts from the last committed
adapter cursor, so already linked native sessions can import the missing tail
without duplicating earlier events. A server or authentication failure before
process launch is fatal; ai-memory does not silently start an unmanaged agent.

## Privacy and storage boundaries

Finish imports, including retries of finished runs, use the identity resolved by
HTTP authentication. A caller must own the run (or reach a shared NULL-owned
run). Database users must hold current Write access to its actual
workspace/project. Another writer in the same project cannot finish a run
attributed to a different
operator. Root without an actor reaches only shared runs; root authentication
does not bypass ownership. Authenticated operators behind the configured trusted
proxy retain their existing project access policy without requiring a database
user. Actor headers without proxy authentication and transcript metadata cannot
supply this authority. Disabling a user's human login does not revoke an active
API key; the HTTP authentication checks the key, and finish checks that the
database user still exists and has current project access.

With authentication disabled but database users still present, anonymous finish
requests to restricted projects are refused. Prepare retains its existing
compatibility path and can still succeed. Completing the run requires
authentication and a caller satisfying the ownership and Write checks above.

The server checks access before reading run status or writing a raw segment,
then checks again in the SQLite writer transaction before any import, cursor,
link, or run update. The transaction reads current users, project mode, creator,
and paired grants; SQL failures and malformed or missing scope fail closed.
This stricter resolution applies only to finish imports. An active run can still
finish after its lease timestamp has elapsed, and an authorized finished retry
imports zero events. Cancelled or replaced runs remain expired and refused.

Raw segments are written before the SQL transaction. A Write revocation after
the preflight can leave a sanitized raw segment without an indexed event. The
writer refusal leaves SQL unchanged; raw-file persistence and SQL are not an
atomic operation.


ai-memory's managed adapters do not write to Claude, Codex, OpenCode, Pi, Crush,
Kimi Code, Command Code, Kiro, OMP, Grok, Antigravity, or Copilot private stores. The
launched harness retains normal ownership of its own session writes. Adapters read only
documented or observed local session formats. Provider credentials, encrypted
content, system/developer prompt records, and hidden reasoning are not copied. The
server sanitizer runs before both the SQLite FTS ledger and immutable files under
`<data_dir>/raw/workstreams/<workstream-id>/segments/` are written.

The ledger is an operational continuity substrate, not a replacement for the
markdown wiki. Durable decisions, rules, procedures, and project facts still
belong in wiki pages through consolidation or explicit durable writes.

## Project and directory renames

`ai-memory rename-project --from OLD --to NEW` changes only the server-side
project name. Wiki paths are UUID-keyed, so it moves no server directory, source
checkout, or native harness session. If the source checkout path itself is
renamed, absolute-path session locators used by Claude Code, Codex, OpenCode,
Pi, Kimi Code (`state.json`'s `cwd` or legacy `workDir`), Command Code (v3
header `cwd`), Kiro v2
(`<uuid>.json`'s `cwd`), Kiro v3 (`session.json`'s `workspacePaths`), OMP,
Antigravity, and Copilot (`session.start`'s `context.cwd`) may still reference
the old path; Crush's project-local `.crush`
database moves with the checkout.

There is no portable, supported API that rewrites every harness's private
project locator. ai-memory therefore does not mutate those stores or silently
equate a renamed checkout with another clone of the same remote. Explicit
native selectors still win and can recover a session when that harness supports
cross-directory resume; OpenCode also provides its own export/import flow. For
a renamed checkout, use an explicit harness and its documented session selector
to seed the new managed workstream. Keep the old checkout path available until
recovery is verified. Automatic discovery intentionally requires the recorded
checkout to match exactly.

## Manual acceptance

The opt-in acceptance runner exercises launcher edge cases and then orchestrates
the locally installed Claude, Codex, OpenCode, Pi, Crush, OMP, Kimi, Command
Code, Grok, Antigravity, and Copilot CLIs through one real workstream:

```bash
scripts/managed-workstream-acceptance.sh
```

It is deliberately separate from CI because it uses local harness credentials
and model calls. Hook configs, native session stores, the ai-memory server, and
the Git fixture are isolated under a temporary directory. Claude, Codex, and
OpenCode receive only copied authentication material; OMP receives a temporary
agent directory with read-consistent credential/model database backups and
copied settings. Crush uses its existing global provider configuration and an
isolated project database. Kimi Code runs with an isolated `$KIMI_CODE_HOME`
seeded with the operator's provider configuration. Command Code runs with an
isolated `HOME` seeded only with `auth.json` and `config.json`. Antigravity runs
with an isolated `HOME` seeded only with the operator's OAuth and settings files.
Copilot runs with an isolated `$COPILOT_HOME` seeded only with `config.json`. The
deterministic phase also covers first-run adoption, bare-mode selection and
empty-directory failure, wrapper `--yolo`, lease exclusion, Crush context
cleanup, fake-mode Kimi and Command Code store/resume/import round trips, an
Antigravity hook/link/resume round trip, a fake-mode Copilot
store/hook/resume/import round trip, a fake-mode Kiro v2
store/resume/import round trip,
the equivalent Kiro v3 nested-store round trip with transparent engine recovery,
private-trajectory exclusion, and the
established-workstream guard against obsolete sessions. The fake Kimi round
trip also deletes the linked native session and verifies automatic
fresh-session recovery and repointing.
Native session creation, read-only extraction, cross-harness injection, and
returning resume paths are all exercised. Docker wrapper host execution and
remote URL preservation are covered separately by the `ai-memory-cli`
packaging tests.

Kiro is intentionally skipped in the scripted real-model loop. Its
`--no-interactive` mode writes a different v1 SQLite store, while both managed
adapters read the interactive v2/v3 journals. Logged-in Kiro acceptance
therefore remains interactive. For v2, run `ai-memory run --new kiro-v2-accept
kiro`, enter a unique prompt, quit normally, then run `ai-memory run
--workstream kiro-v2-accept kiro-cli` and verify the same UUID resumes. For v3,
repeat with a fresh workstream and `kiro --v3`; the second plain `kiro` launch
must transparently add `--v3 --resume-id <sess_uuid>`. Search both workstream
ledgers for the unique visible assistant replies. Record the Kiro version and
sanitize metadata/event files before changing either fixture schema.

The real-harness phase treats the model as the system under transport, not as
the test oracle. For each leg it records the prior ledger sequence, then
requires a newly imported assistant event from harnesses with readable native
transcripts. For Antigravity it instead requires the exact native conversation
link and a new correlated startup-hook observation, because its private
trajectory protobuf is deliberately not decoded. When a context delta is
expected, it first verifies that the prior ledger endpoint is newer
than that harness's delivery cursor, then requires the latest managed run to
report that exact endpoint as `sync_through` with `context_delivered = 1`. It
does not require the model to quote a prior sentinel: Claude Code may
externalize a large hook result to a file, and whether a model chooses to read
that file is not a deterministic continuity signal. The deterministic fake
Grok, Antigravity, and Copilot cross-harness fixtures exercise the same assertion helper
without credentials or model calls.

Set
`AI_MEMORY_ACCEPTANCE_HARNESSES="command-code codex"` to select a
Command-Code-to-Codex-to-Command-Code round trip, or
`AI_MEMORY_ACCEPTANCE_HARNESSES="antigravity codex"` to select an
Antigravity-to-Codex-to-Antigravity round trip (`agy` and `antigravity-cli` are
accepted aliases), `AI_MEMORY_ACCEPTANCE_DETERMINISTIC_ONLY=1` to skip model
calls, or
`AI_MEMORY_ACCEPTANCE_KEEP=1` to retain all temporary logs and data.

## Running inside Herdr

[Herdr](https://herdr.dev/) is a terminal workspace manager that tracks which
agent runs in each pane. It identifies the agent from the pane's foreground
process, falling back to matching the agent's own screen output against
per-agent manifests.

`ai-memory run` sits awkwardly between the two. The foreground process is the
wrapper and the agent is its child — one process group whose leader is
`ai-memory` — so process detection does not find the agent. The pane resolves
only once the harness paints a title Herdr recognizes, which can be well after
launch and may never happen for a harness whose output matches no manifest.
Until then Herdr's agents pane shows nothing for that pane.

ai-memory does not try to fix this from the inside, deliberately. Herdr's hint
for wrapper commands, `HERDR_AGENT`, is scoped to the pane's foreground
process, and a process cannot amend its own environment after exec — so the
wrapper has no way to describe itself to Herdr once it is already running.
Setting the variable on the agent it spawns would put it somewhere Herdr does
not look for it.

Two things work today.

**Name the agent on the command**, where Herdr does look:

```bash
HERDR_AGENT=codex ai-memory run codex
```

**Or install Herdr's own agent integration**, which is the better answer:

```bash
herdr integration install codex
herdr integration install claude
```

An installed integration reports agent identity and lifecycle state over
Herdr's socket and is authoritative regardless of process detection — so the
wrapper stops mattering entirely. It also upgrades what Herdr can show: real
`idle` / `working` / `blocked` signals instead of inferring from the screen,
which cannot reliably see `blocked` at all.

These are separate mechanisms writing to separate files: Herdr's integration
installs its own hook script (`~/.claude/hooks/herdr-agent-state.sh` for Claude
Code, `~/.codex/herdr-agent-state.sh` for Codex), while ai-memory's lifecycle
hooks live in the agent's own config. ai-memory's installer preserves foreign
entries rather than replacing them, so re-running `ai-memory install-hooks`
will not remove Herdr's. Back up the agent's config and diff it after
installing either one if you want to be sure the other survived.
