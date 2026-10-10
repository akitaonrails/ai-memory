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
  actually set in the current environment are forwarded.
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
| `github` (`~/.config/gh`) | credential | when that directory exists | on when present |
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
smart defaults:

```
Enable in the jail (Enter = as marked; numbers flip, e.g. "2 4"; "all" / "none"):
  [x] 1) GitHub CLI credentials        ~/.config/gh, read-only
  [x] 2) AWS credentials               ~/.aws, read-only
  [ ] 3) SSH keys + agent              ~/.ssh read-only + SSH_AUTH_SOCK; needed for git over SSH
  [ ] 4) Docker socket                 ⚠ grants host root
>
```

Enter (or EOF) accepts as marked; row numbers (spaces or commas) flip rows and
redraw the list; `all` / `none` set every row. An unrecognized answer changes
nothing and re-prompts; the third one aborts the launch rather than guessing
what to mount. One summary line then names what the jail gets, grouped into
credentials, capabilities, and the user's own `no-X` entries, then "everything
else in the checklist off" for the rows an explicit selection left out (rather
than spelling out each `--no-X`). With no visible row the
checklist is skipped. The reader takes injected `BufRead`/`Write`, like the §1
prompt, so its grammar is unit-tested without a TTY.

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
- The re-exec still forwards only environment names already set (§2), and
  the toggle flags carry no values or secrets.

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
- Re-exec forwards only environment names that are already set; it introduces
  no new secret surface and does not log token values.
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
