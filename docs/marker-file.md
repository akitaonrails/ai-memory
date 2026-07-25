# Marker file: `.ai-memory.toml`

Declare which workspace (and optionally which project) an agent's
`cwd` belongs to, without depending on the directory's basename.

## Why

ai-memory namespaces every wiki page by `(workspace, project)`. By
default, `workspace = "default"` and `project = basename($cwd)`. That
works for a solo developer in `~/projects/<repo>` but breaks down
for the cases this marker file is built for:

- **Multi-client consultancies** with `~/projects/<client>/<repo>` —
  every client should land in a dedicated workspace, not "default".
- **Work / personal / open-source separation** for solo developers
  who want isolation by life context.
- **Mono-repos** where you'd like all packages under one project
  (instead of basename-of-each-package buckets) — or each package
  under its own project, your call.

The marker file lets you declare these mappings without forking
ai-memory or running CLI commands per directory.

## Where to put it

`.ai-memory.toml` in **any ancestor** of your `cwd`. Lifecycle hooks
walk up from `cwd` toward `$HOME` (or `/` if `$HOME` is unset) and
use the **first** marker found. Closer markers override outer ones. When
a marker is found, hook scripts also forward the current `cwd` so
workspace-only markers can still resolve `project = basename(cwd)` for
handoff lookups.

The marker path is shared by the POSIX/PowerShell hook scripts and the
generated OpenCode / OMP / Pi / OpenClaw TypeScript integrations. In all cases,
hook capture and handoff lookup send the same `cwd`, `workspace`, `project`,
`project_strategy`, `drop_subagent`, `default_global`, `briefing`, and
`briefing_budget` query params to the server when a marker declares them;
handoff lookup also sends `cwd` when no marker exists so the default
`project = basename(cwd)` route works consistently.

## Schema

```toml
# Required.
workspace = "movvia"

# Optional. When present, forces project = "pe-portais" for every
# cwd inside this marker's tree. Omit it to let basename(cwd) drive
# the project name.
project = "pe-portais"

# Optional. Omit it to preserve project = basename(cwd). Set it to
# "repo-root" to derive project from the main git repository root, so
# linked worktrees and subdirectories share one project. Ignored when
# `project` is present.
project_strategy = "repo-root"

# Optional. Opt this project into drop_subagent_captures: set it to "true"
# and the server accepts but does NOT store this project's subagent-session
# captures. A multi-agent harness fans one goal out to many subagent
# sessions whose per-event captures can flood a small instance; scoping the
# opt-in here keeps the drop from affecting other projects on the same
# server. Off by default (absent / "false").
drop_subagent_captures = "true"

# Optional. Broaden this repo's DEFAULT memory recall to every project:
# an unscoped `memory_query` from sessions in this tree behaves as
# `global=true`, and an unscoped `memory_recent` returns the most recent
# pages across every project (each hit annotated with workspace + project).
# Meant for meta-repos that constantly need sibling-project context.
# Explicit args always win — passing `workspace`/`project`/`scopes`/
# `global` overrides this for that call. Off by default. Note: while
# active, unscoped queries return cross-project `global_hits` instead
# of project `hits` + `global_scope_hits` (the `_global` preference
# pages still appear, annotated, among the global results).
[recall]
default_global = "true"

# Optional. Inject a compiled project brief at session start (and after a
# context clear — Claude Code re-fires SessionStart on /clear): the
# session-start handoff fetch also returns this project's pinned /
# `_rules/` / `_slots/` wiki pages (bodies included) plus recently-updated
# page titles, so the agent starts with the architecture context instead
# of re-exploring the codebase. Appended AFTER any pending handoff, and
# unlike the handoff it is not consumed — it is recomposed every opted-in
# session start. Only agents whose session-start hook injects stdout as
# context benefit (Claude Code, Codex, OpenCode, …). Off by default: the
# brief costs tokens on EVERY session start, so opt in per repo.
# Kimi Code note: kimi discards SessionStart hook stdout, so there the
# brief is delivered on the FIRST user prompt of the session instead
# (once per session, same as Claude). Its local delivery markers are created
# only for opted-in repositories and bounded to the 512 newest sessions;
# re-briefing after /clear is not supported in v1.
[briefing]
inject_on_session_start = "true"

# Optional. Char budget for the brief (~4 chars per token). Bodies over
# budget are truncated with a visible note; crowded-out core pages are
# listed by path so the agent can `memory_query` them. Clamped
# server-side to [500, 20000]; defaults to 4000.
max_chars = 4000
```

**Naming rules** for `workspace` and `project`, validated server-side:

- Lowercase ASCII, digits, dots, dashes, underscores
- Regex: `^[a-z0-9][a-z0-9._-]*$`

Anything else is rejected at `get_or_create_workspace` / `_project`
time, surfacing as a hook warning. The shell helper URL-encodes
defensively but the server's regex is the source of truth.

`project_strategy` accepts `repo-root` (or `repo_root`) only. Unknown
values are ignored and behave like the default `basename(cwd)` strategy.

`default_global` and `inject_on_session_start` accept a truthy value
(`true` / `1` / `yes` / `on`, quoted or bare — section-style keys are
parsed leniently); anything else behaves as absent. `max_chars` is a
plain integer.

`drop_subagent_captures` accepts a truthy string (`"true"` / `"1"` /
`"yes"` / `"on"`); any other value, or its absence, leaves this project's
subagent captures stored as usual. Top-level (non-subagent) sessions are
always stored regardless. This is per-project on purpose: there is no
server-global switch, so opting one noisy project in never sheds subagent
captures for the others on a shared instance.

## Capture exclusions

Use the exact per-repository shape `[capture]` plus `ignore_paths = [...]`
below to keep recognized file-tool activity under matching paths out of capture:

```toml
[capture]
ignore_paths = ["private/**", "~/personal-notes/**"]
```

The **nearest** `.ai-memory.toml` is authoritative; marker sections do not
merge. A missing `[capture]` section or `ignore_paths = []` is inactive and
preserves current behavior. `[capture]` accepts only `ignore_paths`: unknown
keys, invalid types/globs/roots, unreadable markers, or a marker over 64 KiB
invalidate the whole capture policy rather than partially applying it.

Patterns match an entire lexically normalized path, not a substring. Use only
`*`, `?`, and `**`; relative patterns are rooted at the marker directory,
relative file-tool paths are resolved from the event's actual `cwd`, and `~/`
expands to the home directory. Prefer forward slashes on every platform.
POSIX matching is case-sensitive; Windows drive/UNC matching is ASCII
case-insensitive. Bounds are 128 patterns, 1,024 characters per pattern, 32
direct candidates and 4,096 characters per candidate, and 1,000,000 bounded
pattern/candidate comparisons.

For fixture-proven direct file tools, ai-memory reads only explicit path fields
and documented direct arrays for multi-file calls. If any candidate matches, the
entire event is **dropped locally** before spool, queue, network, transport
logs, or server storage. With an active policy, recognized search/list tools are
dropped conservatively; missing or malformed recognized file candidates, an
unsupported recognized schema, or an invalid policy become **metadata-only**.
That form contains only bounded routing/tool/decision metadata, never paths,
patterns, arguments, output, errors, titles, or nested payload. Known non-file
and unknown tools retain current behavior. Excluding content before transport
matters because it cannot then reach observations/FTS, session pages, handoffs,
reviewer requests, proposals, or logs.

This is a lexical capture boundary, **not complete DLP**. It does not resolve
symlinks, junctions, bind mounts, or Windows 8.3 aliases. Shell commands and
free-form patches are not parsed; prompts, assistant text, notifications, and
quoted content are not path-attributable. Add each relevant visible alias
explicitly, and do not rely on this feature to detect every way private content
can be mentioned.

### Supported integrations and refresh

Capture policy v1 is enforced by native `ai-memory hook` commands (including
native POSIX/Windows hook commands) and generated OpenCode, OMP, Pi, and
OpenClaw integrations. Local installers default to native commands where that
path is supported. Legacy `.sh`/`.ps1` hooks and remote-only/Docker script
bundles do **not** enforce it. Reinstall hooks or refresh/reinstall generated
plugins after upgrading; existing hooks/plugins keep their prior behavior.
Installer capability output describes the selected integration.

New clients remain safe with old servers because stripping and dropping happen
on the client. Old clients talking to new servers retain old behavior and cannot
enforce a host-only marker policy; the server cannot warn about policy it never
saw. The policy adds no MCP tool and no database migration.

### Check a decision locally

`ai-memory hook --event ... --agent ... --check-capture` reads one JSON payload
from stdin and performs no spool, queue, drain, network, or handoff work. It
prints only bounded decision metadata (protocol version, policy state, tool
family, path count, disposition, and extraction state), never paths, patterns,
or payload content:

```bash
printf '%s\n' '{"session_id":"demo","cwd":"/example/workspace","tool_name":"Edit","tool_input":{"path":"docs/example.md"}}' \
  | ai-memory hook --event post-tool-use --agent claude-code \
      --server-url http://127.0.0.1:49374 --check-capture
```

The normal capture contract is intentionally narrow: supported Claude Code,
OpenCode, Pi, and Antigravity tool events retain only canonical tool family,
an agent-provided validated call ID when their documented schema proves one,
and a PostToolUse outcome class. `PreToolUse` never retains commands,
arguments, paths, input bodies, or arbitrary tool names. `PostToolUse` appends
its existing tool-response/error excerpt and caps the complete rendered body at
2,000 UTF-8-safe bytes. Unsupported tool envelopes do not gain a PreToolUse
body, and association is only by matching agent-provided call IDs. User-prompt stores its prompt
text, notification stores its message/text, and post-compaction stores its
summary; other event bodies are currently empty unless explicitly supported.
Stop/assistant-message capture is disabled by default and never persisted; it is
available only through the explicit double opt-in described in the install guide
(`install-hooks --capture-assistant` on the client plus `capture_assistant` on
the server), where the excerpt is sanitized on both sides and capped. It is not
gated by this marker file — assistant text is not path-attributable, so a
`.ai-memory.toml` cannot narrow it. The metadata header is closed; the
PostToolUse response/error excerpt remains the existing bounded content capture.
Capture exclusions are evaluated only where paths have a proven schema, so they
do not claim to filter those other bodies.

## Four canonical examples

### Multi-client

```
~/projects/movvia/.ai-memory.toml     → workspace = "movvia"
~/projects/cliente-x/.ai-memory.toml  → workspace = "cliente-x"
~/personal/.ai-memory.toml            → workspace = "personal"
```

Outcome:

- `~/projects/movvia/pe-api-core` → workspace = `movvia`, project = `pe-api-core`
- `~/projects/cliente-x/api`      → workspace = `cliente-x`, project = `api`
- `~/personal/blog`               → workspace = `personal`, project = `blog`

### Mono-repo with grouped packages

```
~/projects/movvia/.ai-memory.toml              → workspace = "movvia"
~/projects/movvia/pe-portais/.ai-memory.toml   → workspace = "movvia"
                                                  project   = "pe-portais"
```

Outcome:

- `~/projects/movvia/pe/pe-api-core`        → workspace = `movvia`, project = `pe-api-core`
- `~/projects/movvia/pe-portais/apps/web`   → workspace = `movvia`, project = `pe-portais`
  (closer marker wins)

### Git worktrees / repo-root identity

```
~/projects/.ai-memory.toml → workspace        = "oss"
                            → project_strategy = "repo-root"
```

Outcome:

- `~/projects/ai-memory`                → workspace = `oss`, project = `ai-memory`
- `~/projects/ai-memory/crates/cli`     → workspace = `oss`, project = `ai-memory`
- `~/projects/ai-memory-feature-branch` → workspace = `oss`, project = `ai-memory`

If the marker lives inside the main checkout instead (for example
`~/projects/ai-memory/.ai-memory.toml`), copy or commit it into each
out-of-tree worktree, or place a shared marker above the worktree parent
directory as shown here.

Without `project_strategy = "repo-root"`, those same paths keep the
default behavior and resolve by their current directory basename.

Resolution is host-side: lifecycle hooks and generated TypeScript
plugins follow the worktree's commondir pointer (`git rev-parse
--git-common-dir`, or the same Rust/libgit2 helper for native hooks) to
the main repository and send the resolved name as an explicit `project`.
This means it works even when the worktree directory lives **outside**
the main repo tree (some tools keep worktrees in a separate directory,
so the worktree has no `.ai-memory.toml` ancestor of its own) and even
when the server runs in a container that cannot see the host checkout.
Put the marker anywhere on the walk-up path from the worktree — commonly
a single `~/.ai-memory.toml` — to select the strategy.

### Single workspace, no per-repo overrides

```
~/.ai-memory.toml → workspace = "home"
```

Every cwd under `$HOME` lands in workspace `home` with
`project = basename(cwd)`. Useful when you just want to opt out of
the `default` bucket entirely.

## Migrating existing projects

Projects already created under workspace `default` stay there. Move one to a
different workspace with the CLI:

```sh
ai-memory move-project \
    --from-workspace default --project foo \
    --to-workspace movvia --confirm
```

## Install-wide default (no marker)

`project_strategy = "repo-root"` normally lives in a marker, which means
dropping a `.ai-memory.toml` in (or above) every repo. To get the same
repo-root resolution for a whole install **without** a per-repo marker, bake
it into the generated hooks at install time:

```sh
ai-memory install-hooks --apply --agent claude-code --project-strategy repo-root
```

Every session for that install then resolves its project from the main git
repo root — so an agent that runs `mkdir sub && cd sub` and stays there no
longer forks the rest of the session into a phantom project named `sub`.

This is **install-time config**, written into the agent's hook command (and
the generated OpenCode / OMP / Pi / OpenClaw plugins) — the same status as the
`AI_MEMORY_AUTH_TOKEN` / `AI_MEMORY_HOOK_URL` it sits beside, *not* a user-set
runtime override (which was deliberately rejected in #16). The flag accepts
`basename` (the default — bakes nothing, behavior unchanged) or `repo-root`.

Precedence is unchanged: a marker's explicit `project_strategy` or `project`
still wins over the install default.

## What the marker file does NOT do

- ❌ No glob patterns. Walk-up by literal ancestry only.
- ❌ No merge of ancestor markers. Closest wins.
- ❌ No automatic migration of `default`-workspace projects.
- ❌ No automatic repo-root collapsing. Worktrees and subdirectories only
  share a project when `project_strategy = "repo-root"` is explicitly set
  (per marker, or baked install-wide — see above).
- ❌ No user-set env / auth / hook-url override. Use the existing env vars
  (`AI_MEMORY_AUTH_TOKEN`, `AI_MEMORY_HOOK_URL`) for those. (A repo-root
  *default* can still be baked into an install without a marker via
  `install-hooks --project-strategy repo-root`, but that is install-time
  config, not a runtime override the user sets in their shell.)

## Troubleshooting

**My marker isn't being picked up.** Walk through:

1. File is named exactly `.ai-memory.toml` (note the leading dot).
2. File is in an **ancestor** of the cwd — not a sibling, not a
   descendant.
3. There isn't a closer marker overriding it. Run
   `find ~/projects -maxdepth 5 -name '.ai-memory.toml'` to see all
   markers in your tree.
4. The workspace / project values match the regex above (lowercase
   alphanumerics, dots, dashes, underscores).
5. If you use `project_strategy`, it is exactly `repo-root`.

Hook scripts run fire-and-forget by design, so they don't log on
success. To see what's actually being sent, run a hook script by
hand:

```sh
printf '{"cwd":"%s"}' "$PWD" \
  | sh ~/.local/share/ai-memory/hooks/claude-code/post-tool-use.sh
```

If the marker is being read, the curl line (visible with `set -x`
or in server logs) will include `&workspace=...` in the URL.
