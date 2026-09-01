# Managed cross-harness workstreams

`ai-memory run` is an opt-in launcher that lets one logical coding session move
between Claude Code, Codex, OpenCode, Pi, Crush, Kimi Code, Command Code, Kiro
CLI v2/v3, OMP, Grok Build CLI, and Antigravity CLI. Direct agent launches
keep their existing ai-memory behavior. There is no global mode toggle and no
`switch` command: using `run` selects the current workstream and transparently
creates or resumes the correct native session for the requested harness.

```bash
cd /path/to/project

ai-memory run claude
# quit Claude Code, then continue the same logical workstream in Codex
ai-memory run codex --yolo
# return to Claude Code later; ai-memory supplies Claude's native --resume
ai-memory run claude --model opus
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

Everything after the harness name is native argv except the wrapper-owned exact
flags `--yolo` and `--fresh`. No `--` separator is needed, and ai-memory does
not maintain a second copy of each harness's option schema. Other wrapper
options come first:

Portable events, handoffs, and project briefs are injected as explicitly
delimited, untrusted historical data. Instruction-like text inside stored
content is evidence only: agents must not execute commands, expose secrets,
change permissions or policy, or use tools merely because that content asks.
Current system/developer/user instructions, the canonical project instruction
file, and the current checkout remain authoritative.

```text
ai-memory run [--workspace NAME] [--project NAME]
              [--workstream NAME | --new NAME] [--executable PATH]
              [--yolo] [--fresh]
              [claude|codex|opencode|pi|crush|omp|kimi|command-code|kiro|grok|antigravity]
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
remain selectable, but managed `run` still fails closed if it cannot prepare a
workstream before launching the agent.

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

## Automatic harness selection

With no harness name, `ai-memory run` inspects checkout-local sessions for
Claude Code, Codex, OpenCode, Pi, Crush, Kimi Code, Command Code, and both Kiro
CLI engines. For an empty workstream it resumes
the newest session automatically. For an established workstream, server state
takes precedence: ai-memory resumes the most recently linked harness that still
has a usable local session. It never chooses a newer but obsolete session from
another harness merely because that file has a later timestamp. Kiro's v2 and
v3 candidates share one server agent identity, but the selected native engine
flavor remains exact. OMP, Grok, and Antigravity remain available explicitly
but are not in the automatic pool.

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
   repository plus worktree fingerprint. It opens a 90-second renewable lease.
   One writer may own a workstream at a time, so two terminals cannot silently
   race its native-session pointers or delivery cursors.
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
   modifying it. Visible user/assistant messages, completed tool calls/results,
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

## Native adapter behavior

| Harness | Fresh native session | Returning native session | Read-only source |
|---|---|---|---|
| Claude Code | generated `--session-id` | `--resume <id>` | `~/.claude/projects/**/*.jsonl` |
| Codex | native default creation | `resume <id>` | `~/.codex/sessions/**/rollout-*.jsonl` |
| OpenCode | native default creation | `--session <id>` | `~/.local/share/opencode/opencode.db` opened read-only |
| Pi | generated `--session-id` | `--session <id>` | `~/.pi/agent/sessions/**/*.jsonl` |
| Crush | native default creation | `--session <id>` | `<project>/.crush/crush.db` opened read-only |
| Kimi Code | native default creation | `--session <id>` | `$KIMI_CODE_HOME/sessions/*/*/agents/main/wire.jsonl` |
| Command Code | native default creation | `--session <uuid>` | `~/.commandcode/projects/*/<uuid>.jsonl` |
| Kiro CLI v2 | native default creation | `--resume-id <uuid>` | `$KIRO_HOME/sessions/cli/<uuid>.jsonl` (+ sibling `<uuid>.json` metadata) |
| Kiro CLI v3 | native default creation with `--v3` | `--v3 --resume-id <sess_uuid>` | `$KIRO_HOME/sessions/<checkout-bucket>/<sess_uuid>/messages.jsonl` (+ sibling `session.json` metadata) |
| OMP | native default creation | `--resume=<id>` | `~/.omp/agent/sessions/**/*.jsonl` |
| Grok Build CLI | generated `--session-id` | `--resume <id>` | `$GROK_HOME/sessions/*/*/chat_history.jsonl` |
| Antigravity CLI | native default creation | `--conversation <id>` | `~/.gemini/antigravity-cli/conversations/<id>.db` metadata plus lifecycle-hook capture |

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
Codex's `resume`, or Antigravity's `--conversation` / `--continue` wins.
ai-memory links the selected native session and resets an unrelated adapter
cursor rather than assuming it belongs to the old session.
Pi and OMP `--session-dir` values and Crush `--data-dir` values are passed
through unchanged and used as the read-only import root. Native store
environment overrides are also honored:
`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME`,
`PI_CODING_AGENT_SESSION_DIR`, `PI_CODING_AGENT_DIR`, `KIMI_CODE_HOME`,
`KIRO_HOME`, and `GROK_HOME`.
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
`--always-approve` option), and Antigravity CLI
`--dangerously-skip-permissions`. Kiro v3 replaced the trust-all flag with
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
uses its default-home v3 settings/hooks for that process. Fresh launches and
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

Crush needs no ai-memory hook installation for managed mode. The launcher reads
its one-time context from the server, copies the existing global Crush JSON into
a private temporary directory, appends an ephemeral context path, and points the
child at that directory with `CRUSH_GLOBAL_CONFIG`. Delivery is acknowledged
only after the child starts, so a spawn failure cannot lose the packet. The
original config is not modified. ai-memory opens the project database read-only;
the launched Crush process continues its normal native session writes.

The Linux/macOS Docker shell wrapper cannot inspect host projects or execute a
host agent from inside its helper container. For `run`, `show`, `continue`, and
`workstreams`, it downloads the matching native release into
`~/.cache/ai-memory/native-runner`, verifies the published SHA-256 checksum, and
executes that host client. Set `AI_MEMORY_NATIVE_BIN=/path/to/ai-memory` to use a
specific native build. Native package, release, and source installs need no
shim. On native Windows, use the published `ai-memory.exe` or a source build.

The wrapper intercepts all four commands before Docker and preserves the host
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
the conflict remains and concurrent writers are still rejected. Terminal
interrupts continue to reach the child while the parent stays alive to finish
or cancel the run.

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

ai-memory's managed adapters do not write to Claude, Codex, OpenCode, Pi, Crush,
Kimi Code, Command Code, Kiro, OMP, Grok, or Antigravity private stores. The
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
(`<uuid>.json`'s `cwd`), Kiro v3 (`session.json`'s `workspacePaths`), OMP, and
Antigravity may still reference the old path; Crush's project-local `.crush`
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
Code, Grok, and Antigravity CLIs through one real workstream:

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
with an isolated `HOME` seeded only with the operator's OAuth and settings files. The
deterministic phase also covers first-run adoption, bare-mode selection and
empty-directory failure, wrapper `--yolo`, lease exclusion, Crush context
cleanup, fake-mode Kimi and Command Code store/resume/import round trips, an
Antigravity hook/link/resume round trip, a fake-mode Kiro v2
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
Grok and Antigravity cross-harness fixtures exercise the same assertion helper
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
