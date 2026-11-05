# Design: safer `--yolo` and ai-jail integration

Status: accepted (release/2.5). Tracks the 2.5 feature that makes
`ai-memory run … --yolo` warn before it disarms an agent's safety prompts,
offers to run the session inside [ai-jail](https://github.com/akitaonrails/ai-jail)
when it is usable, and adds an opt-in "true yolo" that implies `--yolo` and,
for Claude Code, also forces `bypassPermissions` over a settings `defaultMode`.

## Motivation

`--yolo` maps to each harness's dangerous-mode flag (`apply_yolo`;
Claude → `--dangerously-skip-permissions`). That is a loaded footgun: it runs
every tool call with no confirmation. Three gaps:

1. **No warning.** A user types `--yolo` and the agent is immediately
   unsupervised, with no reminder of what that means or a way to back out.
2. **No sandbox nudge.** ai-jail exists precisely to contain an unsupervised
   agent, but nothing connects the two: the user must remember to type
   `ai-jail ai-memory run …` themselves.
3. **Claude still pauses.** Even with `--dangerously-skip-permissions`, Claude
   Code still prompts on explicit `permissions.ask` rules and on its own
   command-safety checks (e.g. "Contains brace with quote character (expansion
   obfuscation)"), so an "unattended" yolo run can stall. Anthropic documents
   these under "Actions no mode auto-approves": no permission mode, including
   `bypassPermissions`, skips them.

## Non-goals

- Changing default (non-`--yolo`) behavior. Everything here is gated on
  `--yolo`, except the explicit `--jail` flag (§5), which a user types to ask
  for a jailed run.
- Modifying ai-jail. Detection uses ai-jail's *existing* observable surface
  (see "Already inside ai-jail"). No companion change is required.
- Prompting in any non-interactive path. The warning/offer only appear on a
  real TTY (hook, CI, detached, and piped runs are untouched).

## The four parts

### 1. Yolo warning prompt (all OSes)

Before `apply_yolo` runs in `run.rs`, when `--yolo` is requested **and** the
session is interactive (`stdin` and `stderr` are both terminals, the same
gate the native-session picker already uses) **and** we are not already inside
ai-jail:

```
⚠  --yolo runs every tool call with no confirmation. An agent can delete
   files, run any command, and reach the network unsupervised.
   Proceed? [Y/n]
```

Short, glanceable, `Enter` = yes. A `n`/`no` aborts before the agent spawns
(exit non-zero, nothing launched). EOF/empty ⇒ yes (the default), because a
bare `Enter` is the common case. This never fires in a non-TTY path, so
scripts, hooks, and CI keep working unchanged.

### 2. ai-jail detect + offer (Linux/macOS)

The offer appears only when accepting it can actually work
(`usable_ai_jail`): on Linux or macOS, with the ai-jail binary on `PATH`
(fallback `~/.local/bin/ai-jail`) **and** a sandbox backend candidate available
(`bwrap` on Linux, including `BWRAP_BIN`; `sandbox-exec` on macOS). A no-exec
`ai-jail --dry-run` preflight then applies ai-jail's own backend trust and local
configuration checks before the offer is shown. It never appears on Windows,
where ai-jail is unsupported, even if a file named `ai-jail` happens to be on `PATH`.
When ai-jail is not usable there is no question at all; the run proceeds
directly after the §1 warning. The re-exec runs the exact path this check
resolved, never a bare `ai-jail` re-looked-up through `PATH` (which missed a
`~/.local/bin`-only install after the user had already accepted).

When usable and we are not already jailed, the prompt gains a second question:

```
ai-jail is installed. Re-run this session inside it? [Y/n]
```

On yes, ai-memory re-execs itself under ai-jail instead of spawning the agent
directly:

```
ai-jail --network --agent-state --no-save-config \
        --env AI_MEMORY_SERVER_URL --env AI_MEMORY_HOOK_URL \
        --env ANTHROPIC_API_KEY --env CLAUDE_CODE_OAUTH_TOKEN \
        --env CLAUDE_CONFIG_DIR --env … \
        -- <current_exe> run <harness> … --yolo
```

- **`--` before the wrapped command.** ai-jail refuses one of its own flags
  appearing after the command, because it cannot tell whether
  `ai-jail cmd --network` means the sandbox or the child. `ai-memory run`
  shares flag names with ai-jail (`--env`, …), so without the separator a
  forwarded `run claude --env GH_TOKEN=…` aborted the launch. After `--`
  ai-jail passes everything to the wrapped command verbatim, starting with ai-jail
  2.4.2. Earlier releases' guard ignores `--` (despite its error text
  suggesting it), so a colliding forwarded flag still fails there. ai-memory
  emits the separator regardless, as the documented contract.

- **Re-exec**, not a nested spawn: `std::env::current_exe()` + the original
  `args_os()`. ai-jail forwards the wrapped argv verbatim and already parses
  `ai-memory run <harness>` (it keeps both the `ai-memory` binary and the
  native harness visible under its private home), so the nested launch works.
- **`--network` (not `--allow-host`).** ai-jail only adds `--unshare-net` when
  network is *off* (`bwrap.rs`: `if !network_enabled { push("--unshare-net") }`),
  so `--network` shares the host network namespace and the host-side ai-memory
  server on `127.0.0.1:49374`, which the managed run and the agent's hooks
  both need, is reachable. Filtered `--allow-host` uses a private netns with a
  CONNECT proxy for *external* hosts only, so it would cut the loopback server
  off; we deliberately do not use it. This is soft protection (full egress),
  which is acceptable: the agent needs outbound network for its own model API
  regardless, and an agent with no access is useless.
- **Env passthrough.** ai-jail clears the environment by default and re-adds a
  minimal allowlist, so `AI_MEMORY_*`, `CLAUDE_CONFIG_DIR`, and provider tokens
  are **not** inherited. ai-memory forwards the ones it set (server/hook URL,
  the token vars it already threads) with `--env NAME`. Only names that are
  actually set in the current environment are forwarded, plus routing values
  the outer run resolved from config: an explicit or bind-derived `server_url`
  and a non-empty `base_path`. A bearer read from `config.toml` or
  `[auth].bearer_token_file` is different: copying it into the environment
  would expose server-root authority to the harness. The jailed launch refuses
  that case and asks for a least-privilege `AI_MEMORY_AUTH_TOKEN` or an
  unjailed run. An operator-supplied environment token was already inside the
  child-process boundary and is forwarded unchanged.
- **`--no-save-config`** so ai-jail never writes these flags into the
  project `.ai-jail` (§5).
- **`--agent-state`** so the harness's own login/credentials persist across the
  private home.

If the user declines the ai-jail offer but confirmed `--yolo`, the run
proceeds unjailed (their choice, already warned).

### 3. Already inside ai-jail (skip the warning, avoid double-wrap)

ai-jail exports **no** dedicated sentinel env var, and we chose not to add one
(keeps the two projects decoupled). Detection uses ai-jail's existing
observable surface, per OS:

- **Linux:** ai-jail always sets a fresh UTS namespace with hostname
  `ai-sandbox` (`bwrap.rs`, unconditional). Read `/proc/sys/kernel/hostname`
  (or `uname`/`gethostname`) and compare. Reliable.
- **macOS:** seatbelt has no UTS namespace, so the hostname is unchanged.
  ai-jail forces `PS1` to begin with `(jail) ` on both backends; on macOS that
  is the available signal. Weaker (a shell can overwrite `PS1`), but it only
  ever *suppresses* the warning: a false negative just shows the warning
  again inside the jail, which is safe; a false positive is unlikely because
  ai-memory is exec'd directly by ai-jail, not through an interactive shell
  that would reset `PS1`.
- **Windows:** ai-jail is unsupported, so this always returns "not jailed";
  the ai-jail offer is never shown.

When detected as already jailed, both the warning and the ai-jail offer are
skipped and the run proceeds directly, so a user who typed
`ai-jail ai-memory run … --yolo` sees no extra friction.

### 4. Claude "true yolo" (opt-in, all OSes; recommended only under ai-jail)

Opt-in `[run] claude_true_yolo` (config) / `--true-yolo` (flag) additionally,
**for the Claude harness only**, injects
`--settings '{"permissions":{"defaultMode":"bypassPermissions"}}'` on the
Claude argv. CLI-flag precedence sits above user and project settings, so a
`defaultMode` there (e.g. `auto` or `acceptEdits`) cannot narrow the run.

What it deliberately does **not** claim to do, verified against Claude Code
2.1.280 and its documentation:

- It cannot silence an explicit `ask` rule. Claude Code enforces `ask` and
  `deny` rules in every permission mode, and `--settings` permission arrays
  *union* with the user/project/local scopes instead of replacing them, so an
  empty `ask` array there is a no-op (earlier releases injected one). To run
  without those pauses, remove the `ask` rules from your own settings; `deny`
  rules never pause (they block), so keeping them costs no interruptions.
- It cannot skip Claude Code's built-in command-safety checks.
- Earlier releases also set three `CLAUDE_CODE_DISABLE_*RM*` environment
  variables. Claude Code reads none of them (they are absent from its binary
  and its env-var reference), so they were removed rather than left implying a
  protection that never existed.

True-yolo is documented as "best paired with a clean sandbox," i.e. ai-jail.

Off by default. **`--true-yolo` is a superset of `--yolo`**: it implies
`--yolo` (the §1 warning, the §2 offer, and each harness's dangerous-mode
mapping) and adds the Claude extras above, so passing both is redundant but
harmless. For every non-Claude harness it is simply interchangeable with
`--yolo`. Like `--yolo`, it is recognized anywhere after `run`, including
after native arguments (`run claude --model opus --true-yolo`) where clap
leaves it in the native argv, and it is never forwarded to the harness as an
unknown flag.
The `claude_true_yolo` config key only upgrades a launch that is already
`--yolo`/`--true-yolo`; on its own it never turns an ordinary run into a
permission-bypassing one without the warning.

### 5. Choosing what the jail exposes: `--jail`, `--no-jail`, the checklist

ai-jail keeps credentials and host devices out by default and exposes each
through a boolean toggle (`--X` / `--no-X`). A jailed agent that cannot reach
`gh`, the AWS CLI, or an SSH key often cannot finish the task, so the user
picks what the jail mounts, from the CLI or an interactive checklist, and
pressing Enter does the friendly thing.

**CLI.** Two wrapper flags on `ai-memory run` (and recognized after native
arguments too, like `--yolo`; neither ever reaches the harness):

| Form | Meaning |
|---|---|
| `--jail` | Re-run inside ai-jail now, without the "Re-run inside it?" question or the checklist, using the smart defaults below. |
| `--jail=github,aws,no-mise` | Same, with exactly the listed toggles; `no-X` forces one off (`--no-X`), and every checklist row this host shows that the list does not name is forced off too. |
| `--jail=all` / `--jail=none` | Every checklist row this host shows on / every one of them off. Lists are applied left to right, so `all,no-docker` works. |
| `--no-jail` | Never re-run inside ai-jail; the `--yolo` warning still shows. Conflicts with `--jail`. |

`--jail` needs `=` for its list (`require_equals`), so `ai-memory run --jail
claude` keeps `claude` as the harness. It works with or without `--yolo` and
in non-interactive runs too; that is how a script gets a jailed run. When
ai-jail is not usable on the host (§2) it fails with an error instead of
silently running unjailed. An explicit `--jail` re-execs *before* the managed
run is prepared, so no lease is opened and cancelled outside the jail. Inside
ai-jail (§3) both flags are ignored: no nesting, no error.

There are deliberately **no bare `--github`-style flags** on `ai-memory run`.
They would collide with harness-native flags of the same name (Claude Code
has its own `--worktree`), and wrapper flags after the harness name are
stripped from the native argv: a bare `--worktree` meant for Claude would be
silently taken away from it. The single namespaced `--jail=` list has no such
collision.

**Toggles.** The table (`JAIL_TOGGLES` in `ai-memory-workstream/src/jail.rs`):

| Toggle | Kind | Checklist | Smart default (Enter / bare `--jail`) |
|---|---|---|---|
| `github` (`~/.config/gh`, and from ai-jail 2.15 the active `gh` token, keyring included) | credential | when that directory exists | on when present |
| `aws` (`~/.aws`) | credential | when present | on when present |
| `kube` (`~/.kube`) | credential | when present | on when present |
| `gcloud` (`~/.config/gcloud`) | credential | when present | on when present |
| `docker-config` (`~/.docker/config.json`) | credential | when present | on when present |
| `ssh` (`~/.ssh` read-only + `SSH_AUTH_SOCK`) | credential | when `~/.ssh` exists or an agent socket is set | on only when `git push` to `origin` goes over SSH (`git@host:…`, an `~/.ssh/config` alias `host:…`, `ssh://…`), read from the push URL so an HTTPS fetch URL with an SSH `pushurl`/`pushInsteadOf` still counts |
| `worktree` | capability | only in a linked git worktree | on there |
| `docker` (socket — ⚠ grants host root) | capability | always | off |
| `gpu`, `display` | capability | Linux only | off |
| `pictures`, `tailscale` | capability | always | off |
| `audio`, `x11`, `host-shm`, `terminal-passthrough`, `update-check`, `mise`, `toolchains` | capability | never (`--jail=` only) | not passed |

Why these defaults: every credential is pre-checked when present because a
user who keeps `gh` or AWS credentials on the machine almost always wants the
agent able to use them, and a box shown only when the path exists never mounts
nothing. The credential rows check the exact path ai-jail mounts. `GH_CONFIG_DIR`
is not consulted, because ai-jail 2.5.0 mounts `~/.config/gh` regardless. SSH
is shown but left unchecked for an HTTPS remote, since only an SSH `origin`
makes `git push` need it. `worktree` is pre-checked in a linked worktree,
where without its metadata mounted writable the agent cannot commit. The host
capabilities stay off: they widen the sandbox without being needed for the
usual task.

**Semantics: an explicit selection is exact.** ai-jail's own config can
enable toggles too: the trusted global `~/.ai-jail` may turn on, say,
`docker`. So whenever the user makes a selection, ai-memory states every
visible row explicitly, and ai-jail's CLI flags override its config:

- **Interactive checklist:** every row the user saw is passed as marked:
  checked `--X`, unchecked `--no-X` (`marked_choices`). An unchecked
  `[ ] Docker socket` stays off even when `~/.ai-jail` enables it; the
  checklist is what-you-see-is-what-you-get.
- **`--jail=LIST`, including `none`:** the named entries, in order (later
  entries win), then `--no-X` for every visible checklist row the list did
  not name. "Exactly what it names" holds against any ai-jail config, and
  `none` means none of the checklist rows. Rows that are not visible are never
  forced: an absent credential mounts nothing either way, and CLI-only toggles
  stay out of the invocation unless named.
- **Bare `--jail`:** the user saw no selection, so it only enables the
  smart-default rows (`checked_choices`) and emits nothing for the rest; the
  user's own ai-jail configuration still decides those.

The chosen toggles are emitted after the
`--network --agent-state --no-save-config --env …` baseline and before the `--`
separator.

**A project `.ai-jail` replaces the checklist.** ai-jail reads its project
config only from the invocation directory, which the re-exec inherits from
ai-memory. When a regular file `.ai-jail` exists there:

| | no project `.ai-jail` | project `.ai-jail` present |
|---|---|---|
| offer accepted (no flag) | checklist; every row passed as marked | no checklist, no toggles: the file is loaded as-is |
| bare `--jail` | smart-default rows on; the rest left to ai-jail config | no toggles: the file is loaded as-is |
| `--jail=LIST` | exactly the list; unnamed visible rows `--no-X` | the same exact list, on top of the file (CLI flags override config in ai-jail) |

A project file is untrusted because it lives in the repository the agent works on.
ai-jail lets it disable capabilities but never enable credentials, `docker`,
`agent_state`, and the like, so ai-memory must not pre-check anything on its
behalf either, and an untrusted repository must not be able to steer the
checklist. Only the file's presence is probed (`JailHostFacts::project_config`);
its contents are never read or parsed. The global `~/.ai-jail` is not
consulted: ai-jail writes it for its status-bar preferences, so most users have
one. To get credentials mounted automatically alongside a project `.ai-jail`,
enable them in the trusted global `~/.ai-jail` or pass `--jail=github,…`,
because the project file cannot enable them.

**Why every re-exec passes `--no-save-config`.** On every ordinary run (not
`--dry-run`, not lockdown, saving not disabled) ai-jail merges its CLI flags
into the project config and writes `.ai-jail` back. Without the flag, the
first jailed run would persist ai-memory's transient flags (`--network`,
`--agent-state`, the checked credentials) into the user's repository; every
later run would then find a project file, skip the checklist, and silently
lose the credential mounts a project file cannot enable, and ai-jail would
warn that the project file's `agent_state` is ignored because it weakens the
baseline sandbox. `--no-save-config` sits in the baseline next to `--network`
/ `--agent-state`, before the `--`, guarded by the same `--help` detection (both
2.4.1 and 2.5.0 have it). It is reserved: `--jail=save-config` is refused.

**Checklist.** Without either flag and without a project `.ai-jail`, after the
user accepts the §2 offer, the rows visible on this host are listed with their
marks:

```
🔒 ai-jail: what the agent can reach (Enter keeps the marks)
   your choices from 2026-11-05 · new: worktree
  [x] 1) GitHub CLI credentials     🔑 gh config and its token, read-only · found on this machine
  [x] 2) SSH keys + agent           🔑 ~/.ssh read-only + SSH_AUTH_SOCK; needed for git over SSH · origin pushes over SSH
  [x] 3) Linked worktree metadata   writable, so the agent can commit here · needed to commit here new
  [ ] 4) Docker socket              ⚠ grants host root
  📁 Extra folders: ai-jail (ro)
  numbers flip · f folders · all · none · reset · Enter go
>
```

Each row says why it is recommended when its smart default checks it
(`checklist_reason`). On a terminal the marks and risks are colored: a checked
box green, a credential's note marked 🔑, the Docker socket red. Without a
terminal, with `NO_COLOR`, or with `TERM=dumb` the text is plain, and the marks
and icons still carry the meaning.

Enter (or EOF) accepts as marked; row numbers (spaces or commas) flip rows and
redraw the list; `all` / `none` set every row; `reset` restores the
recommended marks and clears the extra folders; `f` opens the folder picker
below. An unrecognized answer changes nothing and re-prompts; the third one
aborts the launch rather than guessing what to mount. One summary line then
names what the jail gets, grouped into credentials, capabilities, the user's
own `no-X` entries and the extra folders, then "everything else in the
checklist off" for the rows an explicit selection left out. With no visible
row the checklist is skipped. The reader takes injected `BufRead`/`Write` and a
picker callback, so its grammar is unit-tested without a TTY.

**Remembered answers.** The accepted checklist is saved per checkout in
`<data_dir>/jail-choices.json`, keyed by the worktree fingerprint, and the next
offer starts from it: Enter keeps last time's answers, and the header says when
they were saved. A row the saved answers never covered (a newer ai-jail, a
credential that appeared since) keeps its recommendation and is marked `new`.
A saved toggle that is no longer offered, or a saved folder that is gone or no
longer passes the folder rules, is named under "no longer offered" instead of
being dropped silently. The file lives in ai-memory's data directory, never in
the repository, for the same reason ai-memory passes `--no-save-config`: a
project `.ai-jail` may only close what ai-jail opens, so answers saved there
would be ignored or skip the checklist, and a repository must not be able to
grant itself a credential or a mount. A failed save is reported and the
answers apply to that run only. Explicit `--jail=…` runs and project
`.ai-jail` runs neither read nor write it.

**Extra folders.** `--jail-map DIR` and `--jail-rw-map DIR` (repeatable) mount
a folder read-only or read-write, for example a sibling project the agent
should study. They become ai-jail's `--map` / `--rw-map` (2.15), placed before
the `--` like every other sandbox flag. In the checklist, `f` opens a picker
that starts in the checkout's parent: typing filters by fuzzy match, git
repositories come first, Space selects (read-only), Ctrl+W switches the
selected folder between read-only and read-write, Tab or → opens a folder, ←
goes up, and a query starting with `/` or `~` is a path with Tab completion.
Selected folders are remembered with the other answers.

Every folder, from a flag, the picker or the saved answers, passes
`validate_extra_folder` first. A relative path resolves against the checkout,
and `~/` against the home. Refused, with the reason shown:

- a missing path or a file;
- the checkout itself or a folder inside it (already mounted);
- `/` and the home;
- any folder that is, sits inside, or contains `~/.ssh`, `~/.gnupg`, `~/.aws`,
  `~/.kube`, `~/.config/gh`, `~/.config/gcloud`, `~/.docker`,
  `~/.local/share/ai-memory`, or the configured data directory, where a
  jailed agent could rewrite its own memory and tokens.

The rules are checked on the lexical path and again on the resolved one, so a
symlink into `~/.ssh` is refused too, while the lexical path is what ai-jail
mounts. A refused flag folder stops the launch before anything starts. Folder
flags on a run that ends up unjailed (no `--jail`, the offer declined, or a
non-interactive run without `--jail`) are an error that says how to get a
jail, rather than being ignored. An ai-jail without `--map` / `--rw-map` hides
`f` and refuses the flags with "update ai-jail or drop --jail-map". Inside
ai-jail the flags are ignored like `--jail`: the forwarded argv still carries
them, and the folders are already mounted.

**The next-run line.** After an interactive checklist, the launcher hands the
jailed run (`AI_MEMORY_JAIL_NEXT_RUN`, forwarded with `--env`) the
`ai-memory run` line that repeats its choices exactly, and the jailed run
prints it when it quits:

```
Next time, start with the same jail choices:
  ai-memory run --yolo --jail=github,ssh --jail-map /home/u/Projects/ai-jail claude
```

It names `--yolo`/`--true-yolo`, `--no-autowire`, `--workspace`, `--project`,
`--profile`, `--env-file`, the enabled toggles as an exact `--jail=` list (or
`--jail=none`), the folders and the harness. One-off parts stay out: a resume
selector, native arguments (they may hold a prompt), and `--env` values (they
may hold secrets; the line adds a reminder comment instead). The post-run
switch menu (§2 of `managed-workstreams.md`) keeps all of these for the next
harness: it runs inside the same jail.

**Decision table** (`jail_decision` in `run.rs`):

| | already jailed | `--no-jail` | `--jail[=…]` | neither |
|---|---|---|---|---|
| `--yolo` warning (interactive only) | no | yes | yes | yes |
| ai-jail | no (no nesting) | never | re-exec now; error if not usable | offer + checklist when interactive and usable (no checklist with a project `.ai-jail`) |

**Version requirements.** ai-jail rejects an unknown flag, so ai-memory only
passes flags the *installed* ai-jail supports. It runs `<ai-jail> --help` once
and treats a toggle as supported only when the exact token `--X` appears
(tokens split on whitespace and `/`; never a substring match, so `--docker`
and `--docker-config` stay distinct). Unsupported toggles are hidden from the
checklist; one named explicitly in `--jail=` is an error naming the release
that has it. The credential mounts (`github`, `aws`, `kube`, `gcloud`,
`docker-config`) and `toolchains` need ai-jail **2.5.0**; the rest are in
2.4.1.

**Never offered.** `--jail=` refuses the security switches and the flags
ai-memory owns, with "not available through ai-memory; run ai-jail directly":
`private-home`, `lockdown`, `landlock`, `seccomp`, `rlimits`, `systemd-user`,
`inherit-env`, `macos-host-ipc`, `browser`, the audit-log flags (each weakens
or reconfigures the sandbox itself), and `network` / `agent-state` /
`save-config` (set by ai-memory, §2). Someone who needs one runs ai-jail
directly.

**Security notes.**

- A credential mounted into an unsupervised (`--yolo`) agent is usable by it:
  read-only stops it from changing the files, not from using the tokens in
  them. Pre-checking present credentials trades that exposure for a jailed
  agent that can do its job; the checklist shows each one, and `--jail=none`
  or unchecking opts out.
- `docker` grants effective root on the host; it is never on by default and
  its note says so.
- The re-exec forwards only environment names already set plus resolved
  routing values (§2). It never turns a config-file bearer into an environment
  variable, and the toggle flags carry no values or secrets.

### 6. ai-jail 3.0.1+: the wrapper contract

ai-jail 3.0.0 moved its policy out of the checkout (`~/.config/ai-jail`), runs
its own first-run review for known harnesses, and treats unrestricted network
as OPEN. ai-memory's flag-based launch still works there, but the user saw two
checklists, ai-memory opened the full network, and ai-jail had to guess the
harness from ai-memory's argv (a wrapper flag it did not know, such as
`--jail-map`, made it give up). ai-jail 3.0.1 added a wrapper contract
([WRAPPER_INTEGRATION.md](https://github.com/akitaonrails/ai-jail/blob/v3.0.1/docs/WRAPPER_INTEGRATION.md)),
and ai-memory uses it when it can (#2020).

**When.** The installed ai-jail's `--help` lists `--harness` and
`--request-file`, `ai-jail capabilities --json` reports schema 1 with
`harness_flag`, `request_file`, `status_json` and `configure_requests`, and the
harness has an ai-jail preset (`ai_jail_harness_name`: Copilot and Command
Code do not). `capabilities` is never asked of an older ai-jail, which would
sandbox a command by that name. An explicit `--jail` with an automatic harness
keeps the flag launch, because the server picks the harness after the re-exec
decision. Otherwise the launch is the §2/§5 one.

**Launch.** `ai-jail --harness H --request-file F <reach> --no-save-config
<toggles> <folders> -- ai-memory run …`:

- *Requests, not grants.* Harness state (`agent_state`) and each forwarded
  variable name that is set (`env`) go in the manifest with a reason ai-jail's
  review shows. Values never enter it; ai-jail copies them from its own
  environment once approved. A denied request stops the launch (ai-jail exit
  3), so the list stays short and its order fixed, and an unchanged launch
  writes the same file.
- *The manifest file.* `<data_dir>/jail-requests/<sha256(checkout)>.json`,
  written atomically (one link), owner-only, in an owner-only directory.
  ai-jail itself refuses a manifest inside the checkout, a symlink, or a
  group/world-writable file, so a repository cannot pose as the wrapper.
- *Reaching the server.* A server on `localhost` or `127.0.0.0/8` gets
  `--forward-port PORT`: that one host port on the jail's loopback, and the
  network stays filtered. Any other server keeps `--network`, labelled OPEN
  in ai-jail's review: ai-jail's filter reaches LAN and remote services only
  through its HTTP proxy, and ai-memory's hook clients bypass proxies on
  purpose (§2). `[::1]` is not bridged by `--forward-port`, so it also keeps
  `--network`.
- *Still flags.* `--jail=…` toggles and extra folders were chosen in
  ai-memory (the list, the picker, `--jail-map`), which is the consent a
  request would ask for again. `--no-save-config` stays: without it ai-jail
  saves the launch's restrictive flags (an unchecked `--no-ssh`) as permanent
  project constraints, which a later approval in its review cannot lift. The
  review's own approvals are saved either way.

**Preflight** (`settle_jail_review`). `status --json` for the exact launch
argv, then:

| status | terminal | action |
|---|---|---|
| `backend_unavailable` | either | stop, quoting ai-jail |
| `missing_approved_env` / `directory_drift` / `changed_ssh_key` | either | stop, quoting ai-jail, with the review command |
| first run (review not done) | yes | launch; ai-jail's first-run review covers the requests |
| reviewed, requests pending | yes | run `ai-jail configure --harness H --request-file F`, read status again, stop if anything is still pending |
| review needed | no | stop with the review command |
| nothing pending, or status unreadable | either | launch (ai-jail explains any refusal itself) |

A cancelled review (exit 130) stops with "the agent was not started". The
launch itself is an `exec`, so its exit codes reach the caller unchanged.

**Checklist.** ai-jail's review owns credentials, SSH and network, so
ai-memory's screen shows only the extra folders (`f`, `reset`, Enter). The
remembered toggle answers are kept for a later launch that falls back to
flags, and the next-run line is a bare `--jail` plus the folders.

**Tests.** `jail_contract.rs` covers capability parsing (every missing piece
falls back), harness names against ai-jail's accepted list, server reach, the
manifest, argv assembly and the preflight table. `run.rs` covers the review
flow with scripted statuses: approve, deny, cancel, no terminal, drift,
backend, first run, unreadable status. `jail_requests.rs` runs the real
installed ai-jail (when 3.0.1+ is on `PATH`) against ai-memory's manifest and
flags, and checks that it refuses a manifest in the checkout, a
world-writable one and a symlink. `jail_toggles_e2e.rs` drives the built
binary against a fake ai-jail 3.0.1 for the launch argv, the no-terminal stop,
the drift stop and the preset fallback.

## OS support matrix

| Concern | Linux | macOS | Windows |
|---|---|---|---|
| ai-jail available | bwrap | seatbelt | unsupported → no offer |
| already-jailed detect | hostname `ai-sandbox` | `PS1` `(jail) ` | always "no" |
| `--network` loopback reach | shared net ns | profile-allowed | n/a |
| warning prompt | ✓ | ✓ | ✓ |
| true-yolo env vars | ✓ | ✓ | ✓ (incl. powershell var) |

## Code shape

- `ai-memory-workstream/src/jail.rs` (new): pure, OS-aware, dependency-injected
  detection + command construction: `usable_ai_jail(os, lookup)`,
  `inside_ai_jail(env, hostname)`, `build_ai_jail_invocation(exe, args, env_names)`.
  Pure functions so the OS branches and the argv/env assembly are unit-tested
  without a sandbox.
- `ai-memory-cli/src/commands/run.rs`: the interactive prompt + orchestration
  (gate → warn → offer → re-exec or proceed), reusing the existing
  `is_terminal` gate and a `confirm`-style reader.
- `ai-memory-cli/src/config.rs`: `[run] claude_true_yolo: bool` (default false).
- §5: the toggle table, `--help` support detection (`JailSupport`), host facts
  (`JailHostFacts`, fed by `inspect_repository`'s `origin_push_url` /
  `linked_worktree`), `jail_checklist`, and `parse_jail_toggles` live in
  `jail.rs`; `run.rs` owns `--jail`/`--no-jail` parsing, the `jail_decision`
  table, and the checklist reader.
- `ai-memory-workstream/src/harness.rs`: `apply_claude_true_yolo(env, args)`
  next to `apply_yolo`.

## Security considerations

- The warning/offer are **TTY-gated**; no new prompt reaches hook, CI,
  detached, or piped paths (mirrors the interactive-lock and native-picker
  gates).
- Re-exec forwards only environment names that are already set and non-secret
  routing values resolved from config. It introduces no new secret surface and
  does not log token values.
- true-yolo is off by default, Claude-only, and documented as sandbox-first.
- Detection failure fails *open* to the safe side: if we cannot tell we are
  jailed, we show the warning (never silently skip it).

## Testing

- `jail.rs` unit tests: installed/not (injected lookup), inside/outside per OS
  (injected env + hostname), invocation assembly (flags, `--env` only for set
  names, current-exe + argv forwarding).
- `run.rs`: prompt gate is off without a TTY; decline aborts before launch;
  already-jailed skips the prompt; the ai-jail offer is absent when ai-jail is
  not found.
- `harness.rs`: `apply_claude_true_yolo` sets the three env vars + the
  `--settings` arg for Claude and is a no-op for other harnesses.
- CHANGELOG `### Added` (minor); support-matrix + cookbook updated in the same
  change.
- §5: `jail.rs` unit tests cover list parsing, support detection against
  2.4.1/2.5.0 help text, and every smart default with an injected `$HOME`;
  `run.rs` tests cover the decision table, flag stripping via a real clap
  parse, and the checklist grammar. `tests/suite/yolo_ai_jail.rs` feeds every
  toggle the real installed ai-jail advertises through `ai-jail --dry-run`;
  `tests/suite/jail_toggles_e2e.rs` runs the built binary with fake
  `ai-jail`/`bwrap`/`claude` and a mock server (including a PTY run of the
  offer + checklist and of `--yolo --no-jail`).
