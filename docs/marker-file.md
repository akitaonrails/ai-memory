# Marker file: `.ai-memory.toml`

Declare which workspace (and optionally which project) an agent's
`cwd` belongs to, without depending on the directory's basename.

## Why

ai-memory namespaces every wiki page by `(workspace, project)`. By
default, `workspace = "default"`; a valid `upstream`/`origin` remote supplies
the canonical hostless repository-path project name, and only a checkout
without one uses `project = basename($cwd)`. That works for a solo developer in
`~/projects/<repo>` but breaks down
for the cases this marker file is built for:

- **Multi-client consultancies** with `~/projects/<client>/<repo>`:
  every client should land in a dedicated workspace, not "default".
- **Work / personal / open-source separation** for solo developers
  who want isolation by life context.
- **Mono-repos** where you'd like all packages under one project
  (instead of basename-of-each-package buckets), or each package
  under its own project, your call.

The marker file lets you declare these mappings without forking
ai-memory or running CLI commands per directory.

Static MCP clients also use the marker as the repository-owned source for
explicit scope arguments. For safe concurrent use, declare both `workspace` and
`project`: managed routing tells static clients to pass that pair on every
project-scoped tool call because they cannot attach the real lifecycle-hook
session id. Without a marker override, static clients derive the project from normalized
`upstream`, then `origin`, joining the full hostless path with the standard safe
name mapping; only no-valid-remote checkouts use their folder basename.
Session-aware bridges keep automatic current-project routing.

## Where to put it

`.ai-memory.toml` in **any allowed ancestor** of your `cwd`. Lifecycle hooks
walk up from `cwd` toward `$HOME` (or `/` if `$HOME` is unset) and use the
**first** marker found. When cwd is outside `$HOME`, the walk stops at the
nearest checkout root (`.git` file or directory); outside a checkout, only cwd
itself is checked. Closer markers override outer ones. When a marker is found,
hook scripts also forward the current `cwd` and normalized remote identity so
workspace-only markers resolve the canonical repository-path project, with
`basename(cwd)` retained only when no valid remote exists.

A marker whose only content is a `[capture]` section (see
[Capture exclusions](#capture-exclusions) below) is **transparent** to this
walk: `workspace`, `project`, `project_strategy`, and the other forwarded
settings (`drop_subagent_captures`, `[recall] default_global`, `[briefing]`,
`[profile]`, `[handoff]` and `[recovery]` keys) are resolved from the nearest
ancestor marker that declares at least one of them, skipping past any nearer
marker that declares nothing but `[capture]`. A subdirectory marker added only
to exclude some paths from capture therefore no longer resets scope to
`default` / basename for that subtree. `[capture]`/`ignore_paths` itself is
unaffected by this and always comes from the nearest marker, even a
capture-only one.

The marker path is shared by the POSIX/PowerShell hook scripts and the
generated OpenCode / OMP / Pi / OpenClaw TypeScript integrations. In all cases,
hook capture and handoff lookup send the same `cwd`, `workspace`, `project`,
`project_strategy`, `drop_subagent`, `default_global`, `briefing`,
`briefing_budget`, `profile_contribute`, `profile_consume`, `handoff`, and
`recovery` query params to the server as applicable;
handoff lookup also sends `cwd` when no marker exists so the default
`project = basename(cwd)` route works consistently. Every client also sends
`identity` / `identity_src` when the checkout has a repository identity (see
[Repository identity](#repository-identity)), with or without a marker, and
an explicit `identity_style=path|host_path` alongside every remote identity.
Current clients send `path` when no marker/home route overrides it; server-side
omission stays legacy `host_path` for old-client compatibility.
A marker `project` remains the canonical requested name. Its validated `aliases`
travel as one percent-encoded JSON-array `aliases` query value on both hook and
handoff requests, and are ignored unless the checkout supplies a normalized
hostful git-remote identity.

## Create a marker interactively

Run this from anywhere inside a checkout to create a marker at the current
worktree root:

```bash
ai-memory project init
```

The command asks for `workspace` and `project`. Each default is the scope the
CLI and hooks already resolve for the checkout: an inherited marker's value,
else the project derived from the normalized `upstream`, then `origin`, remote
(`github.com/acme/api` → `default` / `acme-api`), else the root directory name.
Accepting the defaults therefore pins the scope your memory already lives in.
Outside Git, the marker is written to the current directory.

A name may not contain `/`, `\`, whitespace or control characters. The
command needs an interactive terminal, never overwrites an existing marker,
and writes only `workspace` and `project`, so every other key keeps its
default until you add it. In a repository, commit the file so every checkout
and worktree routes the same way.

## Schema

```toml
# Required.
workspace = "movvia"

# Optional. When present, forces project = "pe-portais" for every
# cwd inside this marker's tree. Omit it to use the canonical remote path,
# falling back to basename(cwd) when no valid remote exists.
project = "pe-portais"

# Optional. Set it to "repo-root" to derive a remote-less project's name
# from the main git repository root, so
# linked worktrees and subdirectories share one project. Ignored when
# `project` is present.
project_strategy = "repo-root"

# Optional. Pin this checkout's repository identity: the key its captures
# route by, whatever the folder is called and wherever it is checked out.
# Outranks `project` and the git remote. Use it for a directory with no
# remote that must not collide with every other folder of the same name,
# for two checkouts that should share one memory, or for a monorepo
# subdirectory that deserves its own. Case-folded. See "Repository
# identity" below.
identity = "acme/platform"

# Optional. The default "path" names a remote-backed project from its whole
# repository path without the host ("acme-api" for github.com/acme/api).
# Set "host_path" to preserve the pre-v3 folder/split naming behavior.
# Existing identity-backed projects can be promoted in place only by an
# authorized capture/write. See "Naming projects by repository path" below.
identity_style = "host_path"

# Optional. Former names for this same repository in this workspace. At most
# 16 entries, each at most 128 UTF-8 bytes and matching
# ^[A-Za-z0-9][A-Za-z0-9._-]*$. Whitespace is trimmed and duplicates keep their
# first position. A hostful upstream/origin identity must match the stored
# project's identity; otherwise the aliases are refused. Reads use the
# existing UUID without renaming. Hook capture may rename that UUID to the
# canonical `project` above only after write authorization. Aliases are local
# routing hints, are not persisted, never select another workspace, and remain
# supported after the v3 dual-key compatibility window.
aliases = ["former-name", "old-worktree-name"]

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
# `_rules/` / `_slots/` wiki pages plus recently-updated page titles, so the
# agent starts with the architecture context instead of re-exploring the
# codebase. Appended AFTER any pending handoff, and
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

# Optional. Char budget for the WHOLE brief (~4 chars per token), headers
# and footers included. Every core page first gets a short excerpt (its
# frontmatter `abstract:` or `summary:`, else the prose it opens with); what is
# left deepens pages to longer excerpts and then whole bodies, pinned and
# `_slots/` pages first, then the cheapest bodies. A body is shown whole or
# not at all, and a body shorter than its excerpt is always shown whole.
# Four or more unpinned excerpts from one folder (`_rules/`, `gotchas/`,
# `decisions/`, `procedures/`) share a heading. Pages the budget cannot
# cover are listed by path so the agent can `memory_read_page` them,
# degrading to a count when even the paths do not fit. Pin a page to give
# it depth first. The security notice and the untrusted-history markers
# are never traded for content. Clamped server-side to [1500, 20000];
# defaults to 4000.
max_chars = 4000

# Optional. Hand the same brief to each `claude-code` subagent when it starts
# (`SubagentStart`), so a delegated task begins with the project's rules and
# slots too. Independent of `inject_on_session_start`; `max_chars` bounds it,
# and that agent's 10,000-character hook context limit caps it. It comes
# from the read-only `GET /brief`, which never reads or claims a handoff, so a
# subagent cannot take its parent session's baton; a server without that
# route answers 404 and the subagent starts without a brief. Off by default:
# every subagent pays for the brief in tokens, so opt in per repo.
inject_on_subagent_start = "true"

# Optional. Stop delivering this project's handoff at session start. Only an
# explicit false / "no" / "off" / "0" turns it off; the default is to deliver.
# Handoffs are still created when a session ends, and stay open (an agent can
# still take one with `memory_handoff_accept`), so only the automatic delivery
# stops. The brief, the profile digest and everything else on start are
# unchanged.
[handoff]
deliver_on_session_start = false

# Optional. Stop delivering the unfinished-session recovery snapshot at
# session start. Only an explicit false / "no" / "off" / "0" turns it off;
# the default is to deliver. This also suppresses the snapshot's backfill
# marker. The source session remains open and its lifecycle is unchanged.
# Independent of [handoff]: handoffs, managed context, the brief, the profile
# digest and the inbox notice are unchanged.
[recovery]
deliver_on_session_start = false

# Optional. The cross-project profile (docs/cross-project-profile.md): your
# usual choices from other projects, delivered at session start as defaults
# below this repository's rules file. Both keys default to on.
# `contribute = false` keeps this project out of the profile (client or NDA
# work); `consume = false` keeps the profile digest out of its session starts
# and the profile out of its queries. Forwarded by every hook client at
# session start and recorded on the project, so removing a key turns the
# setting back on. Quoted or bare; only an explicit false / "no" / "off" /
# "0" turns a key off.
[profile]
contribute = false
consume = false
```

**Naming** for `workspace` and `project`: the server stores both names
as given and does not validate their characters. Stick to lowercase ASCII
letters, digits, dots, dashes and underscores (`^[a-z0-9][a-z0-9._-]*$`)
anyway: names like that need no quoting in URLs, shells or tool arguments.
Hook clients URL-encode whatever a repository marker declares. The
operator-home route map below is stricter: a `route_workspace` or
`route_project` outside that pattern invalidates the whole map. Names never
become filesystem paths, because the wiki stores each project under
`<data_dir>/wiki/<workspace_id>/<project_id>/`. A name such as `../x` is
an odd label, not a path.

`aliases` is all-or-nothing and intentionally uses a portable subset shared by
native Rust, POSIX shell, PowerShell, and generated TypeScript: exactly one
root-level declaration on one line, containing a JSON-compatible array of
unescaped strings. Duplicate declarations, table placement, multiline arrays,
comments/trailing junk, trailing commas, path-like/control-bearing values,
non-strings, oversized values, and over-count lists make hook and handoff
routing fail closed without echoing their contents. An absent or empty list is
inert. The canonical `project` still wins when it already resolves. Otherwise
every local alias hit must deduplicate to one project carrying the checkout's
exact hostful remote identity; a different identity or more than one project
fails closed. Alias lookup never searches another workspace, and capture may
rename only when `project` equals the canonical path-style name derived from
that exact remote identity.

### Operator-home routes

The exact operator-home marker (`$AI_MEMORY_HOME/.ai-memory.toml`, otherwise
`$HOME/.ai-memory.toml`, with `%USERPROFILE%` as the portable Windows fallback)
may route many unrelated checkouts without placing a marker in each repository:

```toml
[routes.identity."github.com/acme/api"]
route_workspace = "oss"
route_project = "acme-api"
route_identity_style = "path"
route_aliases = ["main"]

[routes.path."~/src/api"]
route_workspace = "oss"
route_project = "acme-api"
route_aliases = ["old-api"]
```

Identity selectors must be exact normalized hostful remote identities; a
hostless `acme/api` selector is invalid. Path selectors must be absolute or
`~/`-relative. They are normalized lexically without resolving symlinks, support
POSIX, Windows drive, and UNC roots, and match by path components: `/team-b`
does not match `/team-bb`. A `~/` selector may normalize within HOME (for
example `~/a/../b`) but is rejected if any `..` would escape above HOME;
absolute selectors retain lexical root clamping. The longest path match wins, while an exact identity
match wins over every path match. A reachable non-home local settings marker
wins over all home routes. When no route matches, existing root-level settings
in the home marker remain the fallback, followed by the default remote-path
identity and then the installed/cwd fallback; explicit CLI scope always wins. A matched route replaces
only the root `workspace`, `project`, `identity`, `identity_style`, `aliases`, and
`project_strategy` routing fields. Root `server`, `drop_subagent_captures`,
`[recall]`, `[briefing]`, `[profile]`, `[handoff]`, and `[recovery]` settings
still apply.

A home route map is bounded to 64 routes, a 64 KiB file, 512 UTF-8 bytes per
selector, and 512 UTF-8 bytes per scalar value. Its portable grammar permits
only exact `[routes.identity."selector"]` and `[routes.path."selector"]` table
headers followed by one declaration of each supported `route_*` field. Workspace
and project are required; style and aliases are optional. Aliases must be a
one-line JSON-compatible string array under the normal `MarkerAliases` limits.
Duplicate raw selectors across either route kind, normalized duplicate paths,
equal normalized identities, duplicate fields, multiline arrays, trailing
commas, escapes, malformed selectors or fields, unknown route keys, invalid
styles, and invalid aliases invalidate the whole route map rather than choosing
a project. Recognizable route syntax includes `[routes`, `routes =`, `routes.`,
and orphan `route_*` declarations; malformed root-only settings with none of
those forms retain the historical line parser behavior. An invalid or oversized map never falls back to root-home scope:
commands without a complete explicit workspace/project pair fail, while hooks
accept the lifecycle call but drop it before spool or network. Route aliases use the same bounds and grammar as local
marker aliases and are forwarded only with a discovered hostful git remote and
`project_src=marker`. The `route_*` child names are deliberate: older line-based
clients ignore them instead of treating route fields as root settings. Portable clients use `HOME`, then `USERPROFILE`; native CLI/hooks honor
`AI_MEMORY_HOME` first. The Docker wrapper passes the host cwd, so path routes
compare in the host namespace rather than `/work` while remote discovery may
fall back to the mounted checkout.

`project_strategy` accepts `repo-root` (or `repo_root`) only. Unknown
values are ignored and behave like the default `basename(cwd)` strategy.

`[profile] contribute` and `consume` are on unless the value is explicitly
falsy (`false` / `0` / `no` / `off`, quoted or bare). A marker that sets only
`[profile]` keys is a settings boundary, like one with `[briefing]` keys.

`[handoff] deliver_on_session_start` is on unless the value is explicitly falsy
(`false` / `0` / `no` / `off`, quoted or bare). Clients send `handoff=0` on the
session-start fetch only when the marker turns it off, so the server reads
nothing, claims nothing and adds no handoff or continuity notice for that
project. A marker that sets only `[handoff]` is a settings boundary, like one
with `[briefing]` or `[profile]` keys. Use it for projects whose automatic
handoffs are mostly noise, such as a bot fleet driven by routines and webhooks.

`[recovery] deliver_on_session_start` is also on unless explicitly falsy.
Clients then send `recovery=0`, which suppresses only the unfinished-session
recovery snapshot and its backfill marker. The source session stays open, and
handoffs, managed context, the brief, the profile digest and the inbox notice
are still delivered. A marker that sets only `[recovery]` is a settings
boundary. Use this in projects with long-lived parallel sessions, where another
session's recovery snapshot is usually noise for that layout.

`[recall] default_global` applies to an agent's unscoped `memory_query` and
`memory_recent` calls. On `memory_query`, an explicit `global: false` keeps
the resolved project, even when the marker enables global recall. Explicit
`workspace` + `project` or `scopes` also override the marker. A query with
`as_of` reads the current project as it stood at that instant, and an explicit
`global=true` with `as_of` is refused. The `ai-memory search` and
`ai-memory recent` terminal commands do not read it: those commands stay
scoped to the current project and cover every project only when you pass
`--global` (see
[usage.md](usage.md#search-from-the-terminal) and
[usage.md](usage.md#recent-pages-from-the-terminal)).

`default_global`, `inject_on_session_start` and `inject_on_subagent_start` accept a truthy value
(`true` / `1` / `yes` / `on`, quoted or bare; section-style keys are
parsed leniently); anything else behaves as absent. `max_chars` is a
plain integer.

The session-start brief is **project-scoped**: it draws only from the
session's resolved `(workspace, project)`. Cross-project knowledge reaches
the session through the **profile digest** instead, a separate fenced section
after the brief (see `docs/cross-project-profile.md`): the profile's entries
are delivered as defaults the agent applies when the user and the
repository's rules file say nothing. A hard rule that must hold in this
repository whatever the defaults say still belongs in the agent's own rules
file (`CLAUDE.md` / `AGENTS.md`), which takes precedence over memory. See the
"Rules, memory and the profile" guidance in `docs/usage.md`. Other `_global`
pages stay reachable on demand through `memory_query`, which unions them.

`drop_subagent_captures` accepts a truthy string (`"true"` / `"1"` /
`"yes"` / `"on"`); any other value, or its absence, leaves this project's
subagent captures stored as usual. Top-level (non-subagent) sessions are
always stored regardless, including Claude Code sessions launched with
`--agent`: `agent_type` alone describes the selected agent, while a non-empty
`agent_id` identifies a subagent. Claude Code reports a subagent under the
parent's `session_id`, so only the events carrying `agent_id` (plus
`SubagentStart`/`SubagentStop`) are dropped and the parent session's own
prompts, Stop, SessionEnd and summary are kept. Grok's `subagentType` marker
still identifies subagent captures. The OMP extension posts every event of a
subagent session with `agent_id`, so the whole subagent session is dropped,
while an advisor's tool calls stay with the session they advise. This is
per-project on purpose: there is no server-global
switch, so opting one noisy project in never sheds subagent captures for the
others on a shared instance.

## Allowlist mode: the marker as an opt-in

By default this file is optional: a repository without one is still captured,
and the marker only *narrows* what is taken. An install can invert that:

```bash
ai-memory install-hooks --apply --capture-mode allowlist
```

Under allowlist mode the presence of a `.ai-memory.toml` **is** the opt-in. A
repository without one emits no lifecycle event at all. Prompts, tool calls
and session boundaries alike are dropped in the hook process before anything
reaches the local spool or the wire. No extra key is needed: an existing marker
already opts its repository in, whatever else it configures.

This changes what forgetting the file costs. Under the default, forgetting
means a repository is captured that you may not have intended; under allowlist
mode it means a repository stays silent that you may have wanted. Pick the
direction whose failure you would rather explain.

The mode is stored per install rather than per agent, and a later bare
`install-hooks --apply` (including an upgrade refresh) leaves it in place.

It is enforced both by native `ai-memory hook` commands and by the generated
TypeScript integrations (`pi`, `omp`, `opencode`, `opencode2`, `openclaw`);
each bakes the selected mode in and carries the same marker-presence gate
before it ever POSTs. Only the raw script-fallback paths (the
`AI_MEMORY_HOOK_PLATFORM` override, the Docker host wrapper, and
`setup-agent` snippets) POST to the server directly without running either
enforcement point, so allowlist mode does not gate them.

## Routing capture to another server (`server`)

One machine can deliver hook capture to more than one ai-memory server, for
example when each organisation you work for runs its own. Register each server
locally under a name, then let a repository's marker select one:

```bash
ai-memory server add team-a --url https://memory-a.example.com --root ~/work/team-a --auth-token-stdin
ai-memory server add team-b --url https://memory-b.example.com --root ~/work/team-b --auth-token-stdin
ai-memory server list      # names, URLs, roots, and whether a token is stored; never the token
```

```toml
# ~/work/team-b/.ai-memory.toml
workspace = "team-b"
server = "team-b"          # a profile NAME, never a URL
```

Profiles live in `<data_dir>/servers.toml`; each token is stored separately in
`<data_dir>/auth-tokens/<name>`, owner-only. A marker, a rendered hook config
and a process command line never contain a URL or token for a profile. A
repository without a `server` key keeps using the server `install-hooks`
configured, exactly as before. `ai-memory uninstall` removes the stored
tokens together with the hooks that read them and keeps the registry, so a
later reinstall lists each profile with `token: missing` until you store one
again.

The marker is repository content, so treat it as untrusted: it can only choose
among servers **you** registered. When several profiles exist, each must
declare `--root` directories, and a marker outside a profile's roots cannot
select it. This is what stops a cloned repository naming another team's
profile.

Every failure is fail-closed. If the named profile is unknown, has no token,
is outside its roots, needs roots it does not have, or `servers.toml` is
malformed, the hook emits **nothing** for that event: nothing is spooled, no
handoff is fetched, and a one-line warning goes to the hook's stderr. It never
falls back to the install default, because that would deliver one team's
capture to another team's server.

Routing is inherited down the tree, unlike the other keys. The nearest marker
that declares `server` decides, so a nested marker that only sets `workspace`,
or only `[capture]`, keeps its ancestor's profile. A nested marker can select
a different profile, but only if that profile's roots admit it. Outside
`$HOME` the search does not stop at the checkout root, so an
organisation-level marker above a repository (`/srv/work/team-b/`) still
routes it. Inside `$HOME` it stops at `$HOME`, as for every other key. Note
that allowlist mode keeps its own nearest-marker rule: outside `$HOME`, a
repository whose only marker is that organisation-level one is routed but not
opted in, so under allowlist mode it emits nothing until it has a marker of
its own.

The selection fails closed on shape too:

- An unquoted or empty `server =` is a selection that fails name validation,
  never a silent "no selection".
- A UTF-8 BOM or a stray non-UTF-8 byte cannot hide the key.
- A marker on the way up that cannot be read is refused
  (`rejected-unreadable-marker`), because it may declare a profile.
- An event whose payload carries no `cwd` is routed by the hook process's
  working directory instead of going to the install default.

Re-running `ai-memory server add` for an existing profile keeps its roots when
`--root` is omitted, so rotating a token cannot lift the restriction. Changing
a profile's `--url` without a new token discards the old token first: a token
belongs to the server it was issued for, so the profile refuses events until
you add one for the new URL. Both `[servers.<name>]` tables and an inline
`servers = { ... }` table can be read and updated by these commands.

Check the decision for a directory without sending anything:

```bash
echo '{"cwd":"'"$PWD"'"}' | ai-memory hook --event user-prompt-submit --agent claude-code \
  --server-url http://127.0.0.1:49374 --check-capture
```

`server_resolution` is `install-default`, `marker`, or one of
`rejected-unknown-profile`, `rejected-no-token`, `rejected-outside-roots`,
`rejected-roots-required`, `rejected-invalid-profile-name`,
`rejected-unreadable-marker` or `rejected-invalid-registry`. `server_profile`
names the profile; no URL, path or token is printed.

**Supported integrations.** Native `ai-memory hook` commands route profiles,
including the session-start handoff fetch, the spooled events drained later,
and the one-time boot backfill. The generated TypeScript integrations
(`opencode`, `opencode2`, `omp`, `pi`, `openclaw`) do not route yet: a
repository whose marker selects a profile emits nothing from them and fetches
no handoff. The script hooks (the `.sh` and `.ps1` bundles used by the
`AI_MEMORY_HOOK_PLATFORM` override, the Docker host wrapper and `setup-agent`
snippets) cannot route either, and likewise drop a routed repository's events
and handoff fetch. Clients older than this change ignore the key and deliver to
their install default, so upgrade every client before adding `server` to a
shared marker.

**Known limits.**

- **Mixed binaries on one data dir.** Spooled events record their profile in a
  field older binaries do not know. If an older `ai-memory` drains the same
  spool, it treats profile events as install-default ones: it may retry a
  rejected one with the install token, or re-send one addressed to a dead
  loopback port to the server in `config.toml`. Upgrade every install that
  shares a data dir.
- **MCP and `ai-memory run`.** Profiles route hook capture only. The MCP
  server entry and `ai-memory run` (its managed-run ledger and heartbeats)
  still talk to the server they were configured with, so in a routed
  repository they reach the install default. Point that repository's MCP
  client at its own server with a per-repository `.mcp.json`, and avoid
  `ai-memory run` there until it learns profiles.

Profiles use static tokens only. OIDC `auth.json` stays with the install
default and is never presented to a profile's server.

**One server, several identities.** Two profiles may share a URL and differ
only in their token. On a shared server that restricts projects per user
(#708), this lets each repository authenticate as the account that has access
to it:

```bash
ai-memory server add team-a --url https://memory.example.com --root ~/work/team-a --auth-token-stdin   # team-a account
ai-memory server add team-b --url https://memory.example.com --root ~/work/team-b --auth-token-stdin   # team-b account
```

Events from different profiles never share a request, and each profile's
retries use only its own token.

## Capture exclusions

Use the exact per-repository shape `[capture]` plus `ignore_paths = [...]`
below to keep recognized file-tool and shell-tool activity under matching paths
out of capture:

```toml
[capture]
ignore_paths = ["private/**", "~/personal-notes/**"]
```

> **`ignore_paths` is a best-effort filter for shell commands and unknown
> tools.** File tools are matched on their explicit path fields, but a shell
> command is only read lexically and a tool the matcher does not list is not
> read at all. The [gaps listed below](#capture-exclusion-residuals) stay
> open: shell variables (`$VAR`), a `cd` inside a command, symlinks, a
> recursive search of a parent directory, unknown tools, and scripts that a
> command reads from a file or a heredoc. For a sensitive project, use
> **allowlist capture mode** (`install-hooks --capture-mode allowlist`, above)
> and leave that repository without a marker. That mode is the strict
> guarantee: nothing from an unmarked repository is captured at all.

A repository that keeps its decision records in the tree (an ADR directory,
a [Keep the Why](https://github.com/oliver-zehentleitner/keep-the-why)
`context/` tree) belongs here too: `ignore_paths = ["docs/adr/**"]` or
`["context/**"]`. The repo owns that record; without the exclusion an agent's
read of it is captured and consolidation compiles it into wiki pages that do
not follow the repo, so the copy is stale the moment the record is superseded
(see the "Repo-native decision records" section of [`usage.md`](usage.md)).

The **nearest** `.ai-memory.toml` is authoritative; marker sections do not
merge. A missing `[capture]` section or `ignore_paths = []` is inactive and
preserves current behavior. `[capture]` accepts only `ignore_paths`: unknown
keys, invalid types/globs/roots, unreadable markers, or a marker over 64 KiB
invalidate the whole capture policy rather than partially applying it. Any
TOML spelling of the table works (`[ capture ]`, `["capture"]`, or a top-level
dotted `capture.ignore_paths = [...]`), and the shapes TOML does not make a
plain table (an inline `capture = { ignore_paths = [...] }`, `[[capture]]`, a
`[capture.x]` subtable, a second `[capture]`) are invalid. The native hook and
the generated TypeScript integrations reach the same decision for each.

Patterns match an entire lexically normalized path, not a substring. Use only
`*`, `?`, and `**`; relative patterns are rooted at the marker directory,
relative file-tool paths are resolved from the event's actual `cwd`, and `~/`
expands to the home directory. Prefer forward slashes on every platform.
POSIX matching is case-sensitive; Windows drive/UNC matching is ASCII
case-insensitive. Bounds are 128 patterns, 1,024 characters per pattern, 32
direct candidates and 4,096 characters per candidate, and 1,000,000 bounded
pattern/candidate comparisons.

A `**/` matches one or more directories, never zero, unlike `.gitignore`:
`**/.env` matches `sub/.env` but not the `.env` at the marker directory, and
`docs/**/x.md` does not match `docs/x.md`. List both forms when you mean both
(`[".env", "**/.env"]`). A trailing `/**` does match the directory itself
(`secrets/**` covers `secrets`).

For fixture-proven direct file tools, ai-memory reads only explicit path fields
and documented direct arrays for multi-file calls. If any candidate matches, the
entire event is **dropped locally** before spool, queue, network, transport
logs, or server storage. With an active policy, recognized search/list tools are
dropped conservatively; missing or malformed recognized file candidates, an
unsupported recognized schema, or an invalid policy (for file, search/list and
shell tools) become **metadata-only**.
That form contains only bounded routing/tool/decision metadata, never paths,
patterns, arguments, output, errors, titles, or nested payload. Tools that read
back output of a command started by an earlier call (Claude Code `BashOutput`
and `TaskOutput`, Codex `write_stdin`, Hermes and OpenClaw `process`, GitHub
Copilot CLI `read_bash`/`write_bash` and their PowerShell twins) become
metadata-only under an active or invalid policy, because nothing proves the
command they read missed every ignored path. For the same reason, a file or
shell tool that reads a file the harness saved an earlier tool's output to is
metadata-only under an active or invalid policy: Claude Code's saved large
results (`~/.claude/projects/<project>/<session>/tool-results/…`) and
background task output (`<tmp>/claude-<uid>/<project>/<session>/tasks/<id>.output`),
OpenCode's truncated output (`<data>/opencode/tool-output/…`) and Pi's
(`<tmp>/pi-bash-<id>.log`). A glob that could match such a file counts too
(`cat ~/.claude/projects/*/*/tool-results/*`): each glob segment fits a
directory or file name it shares a match with, and at least one fitting segment
must spell part of that name's distinctive text (`claude`, `projects`,
`tool-results`, `tasks`, `.output`, `opencode`, `tool-output`, `pi-bash-`)
literally, at least three characters in a row: `tool-res*` and `/tmp/pi-*`
count, while `ls src/*`, `tail -n 50 log/*.log` and `cat /tmp/*.log` (whose
wildcard stands where `pi-bash-` would be) are ordinary globs. Other unknown tools
retain current behavior. GitHub Copilot CLI's runtime `view`, `create` and
`str_replace_editor` names are file tools read from their `path` argument.

Gemini CLI's `read_many_files` is a file tool whose `include` entries (`paths`
in older releases) are globs that Gemini expands case-insensitively, with `**`
reaching any depth. ai-memory reads every entry and drops the whole call when
any one names an ignored path or could expand into one, matching case-insensitively
like Gemini: `docs/adr/x.md`, `docs/*/x.md`, `docs/**` and `**/*.md` are
all dropped under `docs/adr/**`, while `src/**/*.rs` and a root-level `*.md`
are not. A `..` after a `**` is read both ways, since `**` can match no
directory as well as many. Gemini reads an entry that names a directory recursively, and the
matcher cannot tell a directory from a file, so an entry that could contain an
ignored path drops the call too: `docs`, `.` and `docs/` are dropped under
`docs/adr/**`, while `src` and `README.md` are not. Its `exclude` list never
subtracts an ignored path. An entry using glob
syntax the matcher cannot evaluate (`[…]`, `{…}`, `(…)`, `!`) makes the call
**metadata-only**.

Recognized shell tools (`Bash`, `PowerShell`, `shell`, `exec`, `execute_bash`,
`terminal`, Gemini CLI's `run_shell_command`, …) have no path field, so the command line is split into words lexically, the way
a POSIX shell quotes and separates them, without expanding or running anything.
Codex shell calls, including its `exec_command` path, reach hooks as `Bash`
with the command in `tool_input.command`.
A command given as an argument vector keeps each element as one word (a path
with spaces stays whole, up to 256 characters) and also splits each element on
its own. Any word that holds more than one word is split again the same way,
whether it is an argument vector element, a quoted script (`bash -c '…'`,
`sh -c "…"`), or a command substitution inside double quotes (`"$(…)"` or
backquotes), so a script is read like any command line, up to four quoting
levels; a script nested deeper is **metadata-only**. A quoted message that
mentions an ignored path (`git commit -m "…docs/adr/…"`) therefore drops the
event too. An ANSI-C string (`$'…'`) is decoded, a backslash before an
ordinary letter is dropped, and a backslash-newline outside single quotes
joins the two lines, as the shell does. A word with a parameter default
(`${X:-docs/x.md}`, also `:=`, `:+` and the forms without `:`) is also read
with the operand in its place. Under the `PowerShell` tool, or on a Windows
host, the command is also read without PowerShell's backquote and cmd's caret
escape characters. Each
argument that is not a flag, plus the value of a `--flag=value`, `NAME=value`,
or PowerShell `-Param:value` word, plus a value glued to a short flag
(`-fdocs/x.md`), plus the local path of a `file:` URL in any case (cut at its
`?` query or `#` fragment, then ASCII percent-escapes decoded), is resolved like a file-tool path: from the tool's own
`workdir` argument (Gemini CLI's `dir_path`, Antigravity's `Cwd`) when it has one, otherwise from the event's `cwd`.
`~/` (and PowerShell's `~\`) is the home directory and `~+` the working
directory. An argument under `~-`, `~N` or `~name` (the previous directory,
the directory stack, another account's home) names a directory only the
running shell knows, so the command is **metadata-only** under an active
policy. Only a tilde the shell expands counts: a quoted one
(`git commit -m "fix ~approx"`, `grep '~[a-z]+'`), one inside an argument
vector element, or one in a quoted message is literal; one in a script run by
`sh -c` or a command substitution is read. PowerShell resolves `~` itself, so
under the `PowerShell` tool or on a Windows host every tilde counts. A
parameter default nested deeper than four levels makes the command
**metadata-only**, and a backslash escape inside a default
(`${X:-d\ocs/x.md}`) is read as the shell reads it.
The path in a git object name (`HEAD:docs/x.md`, `:docs/x.md`) is resolved
from both the `cwd` and the marker directory, because git reads it from the
repository root. If
one matches a pattern, the whole event is **dropped**, exactly like a matching
file read. An argument containing `*` or `?` also matches when its glob can
reach a pattern's directory: `cat docs/*/0001.md` is dropped under
`docs/adr/**`, `cat *.md` at the repository root is not. A `**` matches no
directory as well as many under zsh and bash's `globstar`, so a `..` after it
is read both ways. bash before 5.2 (macOS ships 3.2) also lets a pattern that
starts with a dot match the `..` entry, so a directory segment that could
(a dot followed by `*`, `?` or a class admitting a dot) is read as `..` as
well. A command that exceeds
the match budget is dropped. A command that no argument matches outright is
**metadata-only** when it uses syntax the matcher does not evaluate: an
unquoted brace expansion (`docs/{adr,x}/1.md`, `{1..3}`), a bracket class
(`docs/ad[r]/1.md`, including one that starts with `]`), a zsh glob group or
extglob (`docs/(a|b)/1.md`, `@(…)`), or a numeric or control escape in an
ANSI-C string. The same holds for those inside a script run by `sh -c`
(also past shell options and their values, as in `bash -o pipefail -c`),
`su -c`, `script -c`, `eval` or a command substitution, but not inside a quoted message or pattern
(`git log --format='%h {a,b}'`). Quoted or escaped braces and brackets,
`${…}`, `{}`, `[ -f x ]`, `$(…)`, `<(…)` and a `( … )` subshell are
unaffected; a word glued to a parenthesis (`f()`, `(a|b)x`) also counts as a
glob group. Inline interpreter code (`python3 -c`, `node -e`, `ruby -e`,
`perl -e`, `php -r`, `lua -e`, `Rscript -e`, `osascript -e`, `deno eval`,
also after options that take a value, as in `python3 -W ignore -c`) is
**metadata-only** under an active policy
unless one of its words names an ignored path outright, which drops it:
code can build a path at run time (`'docs/' + name`), so no lexical match can
rule one out. Hermes Agent's `execute_code` tool, which runs program source
it is given, is metadata-only under an active policy for the same reason.
Running a script file or a module (`python3 -m pytest`,
`node build.js`) is unaffected. A shell command longer than 1 MiB (1,048,576
characters, summed over the elements of an argument vector) is not read at
all: it is metadata-only under an active policy, and its marker lookup
(below) counts as unprovable, which makes it metadata-only even in a
directory without a marker. Variables, command substitution, `cd` state, and
commands that name no path at all (`rg TODO`, `git diff`) are not followed, so
their output is still captured. An invalid policy makes a shell command
**metadata-only**, like a file tool, because a broken marker cannot prove its
arguments miss every ignored path; this holds even when the command cannot be
read. On macOS, whose default filesystem ignores case, the native hook and the
generated integrations match file-tool and shell paths without regard to case
(`NOTES/X.md` matches `notes/**`, `ÄRGER/x.md` matches `ärger/**`) and treat a
decomposed accent like the composed letter, as its filesystem does; other POSIX
hosts compare case-sensitively, and Windows paths always ignore ASCII case.

In a Windows session (the event's `cwd` is a drive or UNC path), a file-tool
path, shell argument or `workdir` is read as the file Windows opens, so other
spellings of an ignored file match its pattern too: Git Bash, MSYS2 and Cygwin
`/c/Users/me/repo/x` and `/cygdrive/c/...` are `C:\Users\me\repo\x`; a rooted
path without a drive (`/repo/x`, `\repo\x`) is on the `cwd`'s drive; the
`\\?\`, `\\.\` and `\??\` prefixes and the local administrative share
(`\\localhost\C$\...`, `\\127.0.0.1\C$\...`) name the drive itself; an
alternate data stream suffix (`private.md::$DATA`, `private.md:stream`) and the
trailing dots and spaces of each segment (`private.md.`, `secrets. \x`) are
dropped. Any other device path (`\\.\pipe\x`) makes a file tool
metadata-only. A native tool reads `/c/x` from the root of the `cwd`'s drive
(`D:\c\x` from `D:\repo`) rather than as Git Bash does, so both readings are
checked and either one matching drops the call. The `cwd`, the marker's
directory and an absolute pattern are read the same way, so a session started
from `\\?\C:\repo`, `\\?\UNC\server\share\repo` or `\\localhost\C$\repo`
matches `docs/private/**` against `C:\repo\docs\private\a.md`, and a pattern
spelled `\\?\C:\repo\docs\private\**` matches it from `C:\repo`. PowerShell
and cmd do not escape with a backslash, so a shell command is also read with
every backslash kept: `Get-Content \\server\share\x`, bare or double-quoted,
names the UNC path. PowerShell's provider prefix is dropped first:
`FileSystem::C:\repo\x` and `Microsoft.PowerShell.Core\FileSystem::C:\repo\x`
(any case) name `C:\repo\x`. A shell argument or `workdir` the matcher cannot
place makes the command metadata-only: a drive-relative path (`C:docs\x`, or a
bare `C:`), which Windows resolves from that drive's own current directory, and
a device path that names no drive or share (`\\?\GLOBALROOT\Device\...`,
`\\.\HarddiskVolume3\...`, `\\?\Volume{...}\...`). A flag such as `/s`, a URL
and an ordinary path stay placeable. A Windows marker cannot hold a POSIX
absolute pattern (`/c/repo/docs/**`, `/docs/**`): no Windows candidate could
match it, so the marker is invalid and fails closed; spell it relative to the
marker or with a drive (`C:/repo/docs/**`). None of this applies to a POSIX session, where `/c/x`, a
trailing dot or a colon is part of the name.

A checkout's exclusions protect its files wherever a call is made from. For
every file-tool target (for `apply_patch`, the files its `*** Add File:`,
`*** Update File:`, `*** Delete File:` and `*** Move to:` headers name, read
after leading whitespace as Codex reads them), every directory, file and
path-like glob argument of a search/list tool (Grep, Glob, LS, `grep_search`,
`glob`, `list_directory`, `search_file_content`, Hermes `search_files` with its
`file_glob` filter, and the other agents' equivalents; a grep `pattern` is a
regular expression and is not read), and every shell argument that spells a
path (one with a separator, after `~/` expansion) plus the tool's `workdir`, the native hook and the
generated integrations find the marker that governs that location, by the
same lexical walk as the event's own, and when it is another checkout's (or the
event's directory has none) apply that marker's policy as well: `cat
../other/secrets.env` or a Grep of `../other/secrets.env` from a sibling
checkout, or a Hermes terminal call from an unmarked directory, is dropped when
`other` ignores `secrets.env` (a search reaching a checkout with an active
policy is dropped, as in its own checkout), and a broken marker there makes the
call metadata-only. Each directory is looked up once, and what one lookup
learns about a directory serves every other location below it, so many paths
in a deep tree cost one check per directory. A call naming more than 4,096
path locations or 16 other markers is metadata-only, and so is a file tool
whose target cannot be read or resolved. The lookups share the glob walk's
budget below (50 ms, and at most 16,384 file checks); a glob's leading
directories are looked up before the walk below them. Locations outside the
event's own checkout are looked up first, and running out of budget there makes
the call metadata-only; inside the own checkout the event's own policy already
applies, so running out there ends the lookup and the call keeps its
disposition.

A location with glob syntax in a directory name (`cat ../*/secrets.env`, `cat
*/secrets.env` from a parent folder, `../{a,b}/x`, `../[ab]/x`)
is looked up from its literal leading directories. When those lie outside the
event's own checkout (the directory holding `.git`), every directory the glob
could pass through is checked for a marker of its own: a `*`/`?` segment lists
the child directories whose names it matches (ignoring ASCII case), a class,
brace or group segment lists every child, and a `**` lists the whole subtree
except the event's own checkout, so `ls ../**/README.md` does not list the
checkout it runs in. A glob whose leading directories lie
inside the event's own checkout is not walked, so `cat packages/*/package.json`
in a monorepo costs nothing however many packages it has. A `.git` at or above
your home directory (a dotfiles repository) or at the filesystem root does not
make that directory the own checkout for these rules, so `cat */secrets.env`
from `~/Projects` is still checked against the checkouts it reaches. Symlinks
are never followed. The walk reads at most 32,768 directory entries, visits at
most 256 directories and runs at most 50 ms per call; past any bound the call
is metadata-only, and so is glob syntax that can leave its leading directories:
a brace or group spanning a separator, a `..` or `~` alternative, a brace
expansion that builds one, or a brace-built directory name that older bash
expands to `..` (`.{*,x}/`). A brace expansion too large to list (more than 64
words) counts as leaving only when its pieces could build such a word; integer
and letter sequences cannot, so `touch test/file{1..200}.txt` and `cat
data/{a..h}{1..9}.csv` are walked like any other glob. A `..` after a `**`,
and a dot-glob directory segment that older bash can expand to `..` (`.*`,
`.?`, a class such as `.[[:punct:]]`, or an extglob such as `.@(.)` or
`.+(.)`), are looked up in both readings, and a zsh glob group or an ANSI-C
numeric escape is looked up as the shell expands it, also inside a
double-quoted script (`bash -c "cat \$'…'"`, `eval "…"`), whose backslash
before `$` the outer shell removes. An older server drops these metadata-only shell and output-reader
events, so upgrade the server before the clients. Excluding content before transport
matters because it cannot then reach observations/FTS, session pages, handoffs,
reviewer requests, proposals, or logs.

This is a lexical capture boundary, **not complete DLP**. It does not resolve
symlinks, junctions, bind mounts, or Windows 8.3 aliases. Shell commands are
matched only lexically (above), and a patch is read only for its file headers; prompts,
assistant text, notifications, and quoted content are not path-attributable.
A copy of a file's content under another path is not linked back to it either.
The harness output files listed above are read back metadata-only under a
policy; to drop those re-reads entirely, add for example
`"~/.claude/projects/**/tool-results/**"` to `ignore_paths` (for every file,
not only the ignored ones). Add each relevant visible alias
explicitly, and do not rely on this feature to detect every way private content
can be mentioned.

<a id="capture-exclusion-residuals"></a>These gaps are inherent to lexical matching and stay open:

- shell variables (`$VAR`) are not expanded, and a command substitution's
  output is not known (its own words are read);
- a `cd` inside a command is not followed (the tool's own `workdir` is);
- symlinks are not resolved, by design;
- a recursive search of a parent directory through a shell (`grep -r x docs`,
  `rg x`) names no ignored path, so its output is kept;
- a script read from a file (`bash run.sh`, `python3 tool.py`) is not read,
  and a heredoc fed to an interpreter (`python3 - <<EOF`) is read only for the
  words it spells out;
- git resolves `REV:path` from the repository root, which is approximated by
  the marker directory and the `cwd`, and a tool's own directory option
  (`git -C dir`, `make -C dir`) is not followed;
- script runners and interpreters the matcher does not list (`awk` with
  `getline`, `watch`, `flock … -c`, `su user -c`) are read only for the words
  they spell out: an ignored path named there still drops the event, but
  brace expansion or inline code inside them is not recognized;
- a bare word naming another checkout's directory (`grep -r x other`) is not
  looked up for that checkout's marker, only arguments that spell a path
  separator are; a recursive search of a literal directory (a Grep whose
  `path` is `..`) does not walk into the checkouts below it, a glob whose
  leading directories lie inside the event's own checkout is not walked, so a
  checkout nested inside it is reached only by a path that names it, and a
  glob is not followed through a symlinked directory; a parameter expansion
  next to glob syntax (`${X}/*/x`) is not walked; a harness output file kept at a
  location other than the defaults listed above (a Claude Code config
  directory whose name lacks `claude`) is read as an ordinary file;
- a saved-output glob counts only when it spells three characters of the
  output file's distinctive name in a row, so a glob with no such literal run
  (`/tmp/p?-b*`) is read as an ordinary glob; requiring the literal text is
  what keeps `cat /tmp/*.log` and `tail log/*.log` ordinary;
- the own checkout is decided after the command ran, so a command that creates
  a `.git` (`touch .git; cat */x`) makes its directory the own checkout and its
  globs are not walked; a `.git` at or above the home directory never counts,
  but one in any other directory does;
- a marker nested inside the own checkout is found only while the lookup
  budget lasts (the shallowest paths are looked up first); past it the
  event's own policy governs the remaining paths;
- a value glued to a short flag is read after the flag's first letter only,
  not after a cluster (`-rf…`), and a `file:` URL's non-ASCII percent-escapes
  are not decoded;
- in a Windows session a shell tool's `workdir` spelled `/c/x` is read only
  as Git Bash reads it (`C:\x`), so arguments resolved from it miss a native
  tool's reading (`D:\c\x` from a `cwd` on `D:`); a file-tool path or shell
  argument spelled that way is read both ways;
- the generated TypeScript integrations check the `[capture]` table as
  strictly as the native hook and reject the marker's structural errors
  anywhere in the file (a line that is neither a header nor a key/value pair,
  an array or inline table left open), but they do not validate other values'
  TOML (an unquoted `x = nope`, a duplicate key in another table), so such an
  error invalidates the policy only in the native hook;
- on Windows only `localhost` and `127.0.0.1` are read as the local machine:
  another loopback or host alias for an administrative share (`\\127.0.0.2\c$`,
  `\\127.1\c$`, `\\<hostname>\c$`) or a `\\wsl$\...` path is matched as the
  UNC path it spells, not as the drive it reaches (administrative shares
  usually need an elevated process);
- Windows matching folds ASCII case only, so a pattern and a path that differ
  in the case of a non-ASCII letter (`É` and `é`) do not match on Windows;
- in a Windows session a drive-relative argument counts as placed when the
  same command also holds a placeable path whose POSIX-shell reading (each
  backslash escaping the next character) spells it, the way Git Bash reads
  `C:\repo\x` as `C:repox`; and a harmless word shaped like `X:y`
  (`sed s:a:b:`) or a device path such as `\\.\NUL` or `\\.\pipe\x` makes a
  Windows command metadata-only;
- tools the matcher does not list keep their current behavior.

For a project whose content must never be captured, use allowlist capture mode
(above) and leave that repository without a marker.

### Supported integrations and refresh

Capture policy v1 is enforced by native `ai-memory hook` commands (including
native POSIX/Windows hook commands) and generated OpenCode, OMP, Pi, and
OpenClaw integrations, including the lexical shell-command matching above.
Local installers default to native commands where that
path is supported. The Linux/macOS Docker wrapper's ordinary `install-hooks`
path uses its checksum-verified native host client and is supported too.
Legacy `.sh`/`.ps1` hooks, explicit compatibility overrides, and
remote-only/manual Docker script bundles do **not** enforce it. Reinstall hooks
or refresh/reinstall generated plugins after upgrading; existing hooks/plugins
keep their prior behavior. Installer capability output describes the selected
integration.

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

The same inspection also reports sanitized `scope` hints (`workspace`, `project`,
`project_src`, `project_strategy`, `identity`, `identity_src`,
`identity_style`, `server_may_remap`) and `scope_resolution`. These are local hints, not a
server lookup: inspection creates no project and may report server-derived
coordinates that the server later remaps. Partial marker scope fails closed.
`event_admits_capture` reports lifecycle eligibility. External producers use
`policy_admits_capture`, which also checks exclusions and server-profile
resolution while ignoring `AI_MEMORY_CAPTURE_OWNER`. The existing
`admits_capture` field still reports native capture admission. Each scope hint
is limited to 512 bytes. Oversized names are omitted (`null`), report
`scope_resolution: "unavailable"` and refuse producer admission; native
routing remains unchanged.
The native hook can derive a project for a marker that declares only
`workspace`. Producer preflight is stricter: declare both names, or provide
`repo-root` so both resolve locally. An absent cwd also returns unavailable
scope and refuses producer admission.

The normal capture contract is intentionally narrow: supported Claude Code,
OpenCode, Pi, OMP, and Antigravity tool events retain only canonical tool family,
an agent-provided validated call ID when their documented schema proves one,
and a PostToolUse outcome class. `PreToolUse` never retains commands,
arguments, paths, input bodies, or arbitrary tool names. The one name kept is
an MCP tool's: a call named `mcp__<server>__<tool>` (Claude Code, Codex, Hermes
Agent) stays in the `unknown` family, with no input or output, and adds
`mcp_server` and `mcp_tool` lines when the whole name is at most 64 ASCII
letters, digits, `_` or `-`. `PostToolUse` appends
its existing tool-response/error excerpt and caps the complete rendered body at
2,000 UTF-8-safe bytes. Antigravity's successful file-edit events fall back to
the bounded replacement or written-content field in `toolCall.args` because its
hook payload does not include an output field; failed edits retain the error
instead. The fallback is Antigravity-only and is discarded with any event
rejected by capture exclusions.
Unsupported tool envelopes do not gain a PreToolUse
body, and association is only by matching agent-provided call IDs. User-prompt stores its prompt
text unless Claude Code hooks were installed with `--no-capture-prompts`;
notification stores its message/text, and post-compaction stores its
summary; other event bodies are currently empty unless explicitly supported.
Stop/assistant-message capture is disabled by default and never persisted; it is
available only through the explicit double opt-in described in the install guide
(`install-hooks --capture-assistant` on the client plus `capture_assistant` on
the server), where the excerpt is sanitized on both sides and capped. It is not
gated by this marker file: assistant text is not path-attributable, so a
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

Without `project_strategy = "repo-root"`, a valid remote still gives every
worktree the default canonical repository-path name. Only remote-less paths
resolve by their current directory basename.

Resolution is host-side: lifecycle hooks and generated TypeScript
plugins follow the worktree's commondir pointer (`git rev-parse
--git-common-dir`, or the same Rust/libgit2 helper for native hooks) to
the main repository and send the resolved name as an explicit `project`.
This means it works even when the worktree directory lives **outside**
the main repo tree (some tools keep worktrees in a separate directory,
so the worktree has no `.ai-memory.toml` ancestor of its own) and even
when the server runs in a container that cannot see the host checkout.
Put the marker anywhere on the walk-up path from the worktree (commonly
a single `~/.ai-memory.toml`) to select the strategy.

### Repository identity

A remote-backed project's default name now comes from its normalized repository
path, while remote-less folders still use their basename. Folder names collide:
two unrelated repositories both checked out as `api/` would otherwise share one
project. On a server with per-project grants (#708) that means one grant, so
every client resolves a **repository identity** for the checkout and sends it
with each event. The first rung that yields one wins:

1. `identity = "…"` in the marker;
2. `project = "…"` in the marker;
3. the `upstream` git remote, else `origin`, normalised
   (`git@github.com:Acme/API.git` → `github.com/acme/api`);
4. the folder name.

Only rungs 1 and 3 change routing. A declared `project` routes by name as it
always has, because a statement outranks the remote: a fork whose marker names
its own project is never filed under the repository it forked from. A bare
folder name routes exactly as before. What routes by identity is an
undeclared checkout with a remote, or a checkout with an explicit
`identity`:

- The project already carrying the identity wins, whatever it is called:
  `~/work/api` and `~/dev/acme-api`, both cloned from `github.com/acme/api`,
  are one project.
- An existing project with that name and no identity yet is **claimed in
  place** by the first capture that may write to it, so upgrading moves no
  memory. A linked worktree (`git worktree add`) claims the project named
  after its main checkout's folder, so it does not matter which checkout
  captures first. The server finds the main checkout from the worktree's git
  metadata, so this needs a server that can read the checkout.
- If the name already belongs to a different identity, the new repository
  gets its own project, named from its owner (`github.com/orgb/api` →
  `orgb-api`, then `orgb-api-2`, …).

The remote is normalised on the host, so credentials embedded in a remote
URL never leave the machine. Other remote names (`fork`, `mine`) are
ignored on purpose: they differ per person, and would give one repository a
different identity for each of them.

#### Naming projects by repository path (`identity_style`)

By default, an undeclared checkout with a valid `upstream` or `origin` remote is
named from the remote's whole repository path without its host (#1033):

```toml
identity_style = "host_path"   # explicit pre-v3 compatibility opt-out
```

With the default `path` style, a project created from a remote is named from the whole
repository path without the host, with `/` written as `-`
(`git@github.com:acme/api.git` → `acme-api`,
`https://gitlab.com/acme/group/api.git` → `acme-group-api`), so every
worktree and clone agrees on the name. Only the name changes: captures still
route by the full identity (`github.com/acme/api`), and the style rides along
only with a remote identity. A declared `project` or `identity` keeps its
own name.

- **Existing projects keep their UUID.** Reads by the canonical path name or
  the v2 basename are find-only and never rename. An authorized default-style
  capture or explicit write through the canonical path name promotes only
  `projects.name` in one transaction; pages, sessions, grants, handoffs and
  every other dependent row remain attached to the same UUID. The v2 basename
  remains readable through v3.
- **Another forge is never merged in.** When the path name is already held by
  a different repository (typically the same path on another forge,
  `gitlab.com/acme/api` after `github.com/acme/api`), the newcomer falls
  back to the name it would get without the style.
- Local marker aliases and operator-home identity/path routes keep their
  documented precedence over the default and remain available for migration or
  deliberate remapping.
- Static CLI/MCP clients still pass only `workspace` + `project`. The thin CLI
  derives the canonical path name when no marker/home route overrides it; other
  static clients should use the same name from `upstream`, then `origin`. A valid
  remote wins `project_strategy = "repo-root"`; the strategy applies only when
  no valid remote exists. They may use
  the canonical path name (`acme-api`) or the v2 basename (`api`) while it is
  unambiguous. A hostless key shared by multiple forges fails closed.
- `ai-memory doctor` combines the effective local marker and normalized
  repository evidence with the server's read-only coordinate diagnosis. It
  reports exact/canonical/legacy compatibility, missing or ambiguous status,
  rename eligibility, and collision reasons. It parses the routing marker once,
  honours CLI scope flags before marker names, and reports only a safe marker
  status/source class; invalid marker TOML is ignored without echoing its
  contents, raw marker paths are never rendered, and remote credentials never
  leave the client. An ambiguous result can name the preferred identity-backed
  candidate UUID/current name for context, but is never a unique resolution or
  rename-eligible result.
- The UUID-keyed project `_meta.md` persists optional `identity`,
  `identity_source`, `canonical_name`, and `legacy_name` fields. A clean
  `reindex` restores the original UUID/name and recomputes the indexed keys
  from the validated identity. Older manifests without these fields remain
  valid and rebuild an identity-less project.

### Single workspace, no per-repo overrides

```
~/.ai-memory.toml → workspace = "home"
```

Every cwd under `$HOME` lands in workspace `home`; a valid remote uses its
canonical path name, while a repository without one uses `project = basename(cwd)`.
Useful when you just want to opt out of
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
repo root, so an agent that runs `mkdir sub && cd sub` and stays there no
longer forks the rest of the session into a phantom project named `sub`.

This is **install-time config**, written into the agent's hook command (and
the generated OpenCode / OMP / Pi / OpenClaw plugins), with the same status as the
`AI_MEMORY_AUTH_TOKEN` / `AI_MEMORY_HOOK_URL` it sits beside, *not* a user-set
runtime override (which was deliberately rejected in #16). The flag accepts
`basename` (the new-install default, which bakes nothing) or `repo-root`. A later
`install-hooks --apply` without the flag preserves the value already baked into
ai-memory's hooks; pass `--project-strategy basename` explicitly to remove it.

Precedence is unchanged: a marker's explicit `project_strategy` or `project`
still wins over the install default.

## Mid-session navigation: `[routing] mid_session`

Everything above decides where a session *starts*. A long session also moves:
an agent runs `cd` into a scratch directory, or into a sibling checkout to
grep a reference. `[routing] mid_session` in `config.toml` decides how those
mid-session events are attributed. It is server-side runtime config, not a
marker key.

```toml
[routing]
mid_session = "follow-cwd"   # default
# mid_session = "sticky"
```

- **`follow-cwd`** (default, historical behavior) re-resolves every
  mid-session event from its own cwd. A `cd` into a sibling checkout records
  those observations in that checkout's project, so one session's raw record
  is split across two projects while its session row and compiled page stay
  in the first.
- **`sticky`** keeps the session's project wherever the agent wanders. This
  matches the model the rest of the system already uses: `sessions.project_id`
  holds exactly one value, and consolidation reads by `session_id` and writes
  one page in the session's project. Choose it when one agent session means
  one project.

Two guarantees hold in **both** modes:

- **A marker still wins.** A `.ai-memory.toml` naming a project is a
  deliberate rescope, not drift, so it is never overruled. The hook tells the
  server which kind of override it sent (`project_src=marker` vs
  `project_src=repo-root`), which is what lets `sticky` overrule a derived
  name while honoring a declared one. A client older than v1.27 sends no
  provenance, and its overrides stay authoritative.
- **Broad anchors never stick.** A session rooted at `/` or at `$HOME` is not
  a meaningful anchor, so it never captures events beneath it. Otherwise one
  stray session started in `$HOME` would fold every project into a single
  bucket.

Session-creating events are unaffected in both modes: opening a session in a
plain non-git folder still names the project after that folder.

Some harnesses report a cwd other than where a tool ran: a subagent keeps its
parent session's cwd while using a file tool in another checkout, and Hermes
Agent always reports its own process directory (usually `~`) while each tool
call names its repository through `workdir` or `path`. Native `ai-memory hook`
commands compensate before applying `follow-cwd` or `sticky`: a tool call's
absolute location (a fixture-backed file tool's target paths, a search/list
tool's explicit `path`, or a shell command's structured `workdir`) routes the
event when every path proves the same repository or marker boundary. That
destination's marker decides allowlist admission, and its capture policy and
`server` profile apply, so a cross-project call cannot use the source
repository's credentials. The source's exclusions still bind: if the payload
cwd's own `ignore_paths` would drop the event (a pattern can name a path
outside its checkout, and a command can name a source file while its `workdir`
points elsewhere), the event is not rerouted and that exclusion drops it.
Relative paths and workdirs, mixed-project calls, unknown schemas, and absolute
paths outside a recognized repository/marker keep the payload cwd. Free-form
shell command text is never reinterpreted as a project routing instruction;
only the structured `workdir` field counts. This changes raw observation
attribution only; the session row and its compiled session page remain in the
project where the session began.

Before that lookup, a Hermes event takes `TERMINAL_CWD` (the session's
`terminal.cwd`) in place of the process directory. A multiplexing Hermes
gateway exports only its default profile's `terminal.cwd`, so a gateway event
of a named profile routes instead by a marker in that profile's own Hermes
home, `~/.hermes/profiles/<name>/.ai-memory.toml` (under the gateway's
`HERMES_HOME` when set). It is a normal local marker: `workspace`, `project`,
`[capture]` and `server` all apply. Without it the event keeps the gateway's
directory. See [install.md](install.md#hermes-agent-nous-research).

Independently of this setting, under `project_strategy = "repo-root"` a
mid-session event whose cwd is outside any git repo *and* any marker (agent
scratch directories, `/tmp`, data folders) already inherits the session's
project rather than minting a phantom project named `scratchpad` or `data`.
The host hook resolves repo-root itself, so a missing override already proves
the cwd resolved to nothing.

## Who reads the marker

Both entry points, as of v1.20:

- **Lifecycle hooks** forward the marker's fields to the server on every
  event, so session captures land in the declared scope.
- **Client CLI commands** resolve `(workspace, project)` locally before
  calling the server: `run`, `bootstrap`, `search`, `read-page`,
  `write-page`, `lint`, `curator`, `embed`, `pending-writes`,
  `forget-sweep`, `auto-improve`, `purge-project`, `rename-project`,
  `move-project` and `move-session --from-project` (source side), and friends.

Before v1.20 only the hooks read it. A checkout declaring
`workspace = "acme"` therefore had its captures land in `acme` while every
CLI command resolved into `default`: the same repository split across two
scopes, with `ai-memory run`'s managed workstream on the wrong side of the
split.

Each field is resolved independently:

1. The explicit flag (`--workspace` / `--project`).
2. The nearest marker: explicit `workspace`/`project`/`identity`; otherwise a
   valid `upstream`/`origin` remote's canonical path name wins, even when
   `project_strategy = "repo-root"` is set. The strategy selects the main repo
   root only when no valid remote exists.
3. The fallbacks: workspace `default` and the current cwd basename. A git
   subdirectory does not imply repo-root without an explicit strategy.

When rung 2 decides a field, the command prints one line to stderr naming
the resolved scope, which half (or halves) the marker decided, and the
marker that decided it:

```console
$ ai-memory search "scope resolver"
ai-memory: scope acme/api (workspace + project from /Users/dev/projects/acme/.ai-memory.toml)
```

`AI_MEMORY_IGNORE_MARKER=1` skips rung 2 for one invocation, restoring the
pre-v1.20 resolution without editing or leaving the marker's tree. It
applies to **client commands only**. The lifecycle hooks still forward the
marker's fields on every event, so an invocation run with it set resolves
into a different scope than the session captures around it. Use it for
one-off reads, not as a way to relocate a repository's memory.

`ai-memory serve` is deliberately excluded: the server has no caller cwd to
walk up from, and its `--workspace` / `--project` are the baked fallback for
hook events that arrive without a usable one.

## What the marker file does NOT do

- No glob patterns. Walk-up by literal ancestry only.
- No merge of ancestor markers. Closest wins. (`server` is the one key
  inherited from the nearest marker that declares it; see above.)
- No automatic migration of `default`-workspace projects.
- No automatic repo-root collapsing. Worktrees and subdirectories only
  share a project when `project_strategy = "repo-root"` is explicitly set
  (per marker, or baked install-wide; see above).
- No URL or token in the marker. `server = "<name>"` selects a server
  profile registered locally with `ai-memory server add`; it cannot introduce a
  new destination or carry a credential. Otherwise use the existing env vars
  (`AI_MEMORY_AUTH_TOKEN`, `AI_MEMORY_HOOK_URL`). (A repo-root
  *default* can still be baked into an install without a marker via
  `install-hooks --project-strategy repo-root`, but that is install-time
  config, not a runtime override the user sets in their shell.)
- No reach outside the trust boundary. The walk stops at `$HOME`; a
  checkout outside it stops at that checkout's root and needs a marker inside
  the checkout. A non-git directory outside `$HOME` needs a marker in its
  exact cwd.

## Troubleshooting

**My marker isn't being picked up.** Walk through:

1. File is named exactly `.ai-memory.toml` (note the leading dot).
2. File is in an **ancestor** of the cwd, not a sibling or a
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
