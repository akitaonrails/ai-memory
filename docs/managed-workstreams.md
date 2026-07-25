# Managed cross-harness workstreams

`ai-memory run` is an opt-in launcher that lets one logical coding session move
between Claude Code, Codex, OpenCode, Pi, Crush, Kimi Code, and OMP. Direct
agent launches
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
# or omit the harness and continue the newest usable session automatically
ai-memory run
```

Everything after the harness name is native argv except the wrapper-owned exact
flag `--yolo`. No `--` separator is needed, and ai-memory does not maintain a
second copy of each harness's option schema. Other wrapper options come first:

```text
ai-memory run [--workspace NAME] [--project NAME]
              [--workstream NAME | --new NAME] [--executable PATH] [--yolo]
              [claude|codex|opencode|pi|crush|omp|kimi] [native arguments...]
```

The default is the most recently selected workstream for the current repository
and worktree, creating one named `default` on first use. `--new NAME` starts an
independent line of work; `--workstream NAME` returns to one. These are optional
branching controls, not harness-switch controls.

## Automatic harness selection

With no harness name, `ai-memory run` inspects checkout-local sessions for
Claude Code, Codex, OpenCode, Pi, Crush, and Kimi Code. For an empty workstream
it resumes
the newest session automatically. For an established workstream, server state
takes precedence: ai-memory resumes the most recently linked harness that still
has a usable local session. It never chooses a newer but obsolete session from
another harness merely because that file has a later timestamp. OMP remains
available explicitly but is not in the automatic pool.

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
   message before the turn. Direct launches continue to use the existing
   single-use handoff path.
4. When the child exits, ai-memory reads the native transcript store without
   modifying it. Visible user/assistant messages, completed tool calls/results,
   compaction summaries, and a non-mutating Git checkpoint enter an append-only
   workstream ledger. Hidden reasoning and unsupported/private records are
   excluded and recorded as extraction-loss annotations.
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
| OMP | native default creation | `--resume=<id>` | `~/.omp/agent/sessions/**/*.jsonl` |

An explicit native selector such as Claude's `--resume`, OpenCode's `--session`,
or Codex's `resume` wins. ai-memory links the selected native session and resets
an unrelated adapter cursor rather than assuming it belongs to the old session.
Pi and OMP `--session-dir` values and Crush `--data-dir` values are passed
through unchanged and used as the read-only import root. Native store
environment overrides are also honored:
`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME`,
`PI_CODING_AGENT_SESSION_DIR`, `PI_CODING_AGENT_DIR`, and `KIMI_CODE_HOME`.
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
Crush `--yolo`, and Kimi Code `--yolo`. OMP currently needs no added flag.
ai-memory does not add a duplicate when the translated native flag is already
present.

Managed support is intentionally narrower than the general integration matrix.
Gemini CLI, Devin CLI, Cursor, Grok Build CLI, and other agents may
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
`state.json`'s `workDir` and never parses the bucket name. Event ids derive
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
contract was verified against Kimi Code v0.29.0. The managed launcher accepts
`kimi`, `kimi-code`, and `kimi-cli`; all three resolve the installed `kimi`
executable.

Crush needs no ai-memory hook installation for managed mode. The launcher reads
its one-time context from the server, copies the existing global Crush JSON into
a private temporary directory, appends an ephemeral context path, and points the
child at that directory with `CRUSH_GLOBAL_CONFIG`. Delivery is acknowledged
only after the child starts, so a spawn failure cannot lose the packet. The
original config is not modified. ai-memory opens the project database read-only;
the launched Crush process continues its normal native session writes.

The Linux/macOS Docker shell wrapper cannot execute a host agent from inside its
helper container. For `run` only, it downloads the matching native release into
`~/.cache/ai-memory/native-runner`, verifies the published SHA-256 checksum, and
executes that host client. Set `AI_MEMORY_NATIVE_BIN=/path/to/ai-memory` to use a
specific native build. Native package, release, and source installs need no
shim. On native Windows, use the published `ai-memory.exe` or a source build.

The wrapper intercepts `run` before Docker and preserves the host `PATH`,
`AI_MEMORY_SERVER_URL`, and authentication environment. The native client's
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

If the client is terminated without cleanup, such as with `kill -9`, its lease
expires within 90 seconds. A later managed run starts from the last committed
adapter cursor, so already linked native sessions can import the missing tail
without duplicating earlier events. A server or authentication failure before
process launch is fatal; ai-memory does not silently start an unmanaged agent.

## Privacy and storage boundaries

ai-memory's managed adapters do not write to Claude, Codex, OpenCode, Pi, Crush,
Kimi Code, or OMP private stores. The launched harness retains normal ownership of its own
session writes. Adapters read only documented or observed local session formats.
Provider credentials, encrypted content, system/developer prompt records, and
hidden reasoning are not copied. The server sanitizer runs before both the
SQLite FTS ledger and immutable files under
`<data_dir>/raw/workstreams/<workstream-id>/segments/` are written.

The ledger is an operational continuity substrate, not a replacement for the
markdown wiki. Durable decisions, rules, procedures, and project facts still
belong in wiki pages through consolidation or explicit durable writes.

## Project and directory renames

`ai-memory rename-project --from OLD --to NEW` changes only the server-side
project name. Wiki paths are UUID-keyed, so it moves no server directory, source
checkout, or native harness session. If the source checkout path itself is
renamed, absolute-path session locators used by Claude Code, Codex, OpenCode,
Pi, Kimi Code (`state.json`'s `workDir`), and OMP may still reference the old
path; Crush's project-local `.crush` database moves with the checkout.

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
the locally installed Claude, Codex, OpenCode, Pi, Crush, OMP, and Kimi CLIs
through one real workstream:

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
seeded with the operator's provider configuration. The deterministic phase
also covers first-run adoption, bare-mode selection and empty-directory
failure, wrapper `--yolo`, lease exclusion, Crush context cleanup, a fake-mode
Kimi store/resume/import round trip, and the established-workstream guard
against obsolete sessions. Native session creation, read-only extraction,
cross-harness injection, and returning resume paths are all exercised. Docker
wrapper host execution and remote URL preservation are covered separately by
the `ai-memory-cli` packaging tests. Set
`AI_MEMORY_ACCEPTANCE_HARNESSES="kimi-cli codex"` to select a
Kimi-to-Codex-to-Kimi round trip (Kimi aliases normalize to the installed
`kimi` executable), `AI_MEMORY_ACCEPTANCE_DETERMINISTIC_ONLY=1` to skip model
calls, or
`AI_MEMORY_ACCEPTANCE_KEEP=1` to retain all temporary logs and data.
