# OpenCode V2 capture for server-profile repositories (#2122)

## Problem

A repository whose `.ai-memory.toml` selects a named server profile
(`server = "<profile>"`, #992) is captured only by the native `ai-memory
hook`: the profile's URL and per-profile token live behind the native
resolver, and no generated integration holds them. The three capture front
doors enforce this by *dropping* a routed repository rather than delivering it
to the install default: the shell bundles (`hooks/_lib.sh`
`ai_memory_server_routed`), PowerShell (`Test-AiMemoryServerRouted`), and the
generated TypeScript (`ts_capture_policy_v1`'s `captureServerRouted`, consumed
as `disposition: "drop"`).

The generated OpenCode V2 plugin is a TypeScript front door. For an
OpenCode V2 session working inside a profile-routed repository, every
lifecycle event is dropped and the session-start handoff is never fetched. The
user has configured a profile for that repository, the native hook could
capture it, and the session is silently invisible to memory.

This is the one front door that *can* reach the native binary: the V2 plugin
already spawns `ai-memory hook` for the assistant-capture Stop path
(`install_hooks.rs` `postStop`). The gap is that only the Stop path, and only
under the `--capture-assistant` opt-in, uses that native route; ordinary tool,
prompt and session events go through the HTTP `postHook`, which drops.

Related: #992 (server-profile routing), #2016 (profile token hardening).

## Goal

Let a profile-routed OpenCode V2 repository be captured through the native
hook, preserving the #992 invariant exactly: its events and its handoff reach
only the profile's server, authenticated only with the profile's token, and
never fall back to the install default. A repository without a `server` key
keeps its existing direct-HTTP capture unchanged.

Scope: the generated OpenCode plugins, **both V1 and V2**. They share the
capture prelude, so the routed-native branch lands in the shared `postHook` /
`fetchHandoff` and both inherit it.

Non-goals: changing the shell or PowerShell front doors (they stay drop-only);
changing Codex, Claude Code or any other generated plugin; cross-profile
session moves (rejected, as today).

## Current architecture (grounding)

All anchors are current as of `release/3.5` at the time of writing.

- **V2 plugin generation.** `build_opencode2_plugin`
  (`install_hooks.rs:4427`) reuses the V1 capture prelude and swaps the host
  binding `OPENCODE2_BINDING` (`:4527-4768`). The native exe path constant
  `NATIVE_HOOK` is baked only under `capture_assistant` (`:4475-4483`);
  otherwise it is `undefined`.
- **The shared `postHook`.** The prelude's `postHook(event, payload)` runs
  `applyMarkerParams` then `capturePolicy`, and returns early on
  `disposition === "drop"` before `enqueueHook` (`install_hooks.rs:5680-5683`).
  All V2 events route through it: `user-prompt` (`:4697`), `session-start` on a
  move (`:4660`), `pre-tool-use` (`:4748`), `post-tool-use` (`:4758`),
  `stop` fallback (`:4593`).
- **The TS drop.** `capturePolicy` (`render_shared.rs:838`) returns
  `{disposition:"drop"}` when `captureServerRouted(cwd)` (`:817`) finds a
  `server =` marker (unreadable marker counts as routed). Module banner
  `CAPTURE_POLICY_V1 = 1` (`:496`).
- **The handoff gate.** The `context` hook calls `fetchHandoff`
  (`OPENCODE2_BINDING:4714`); the generated `fetchHandoff`
  (`install_hooks.rs:5689`) is gated by the same server-routed check, so a
  routed repo fetches no handoff.
- **The native route (already correct).** `ai-memory hook` computes its route
  once with `resolve_hook_route` → `server_profiles::route_for_cwd`
  (`hook.rs:1851`, `:3202`). A `HookRoute::Profile` uses the profile URL +
  token and never falls back (`hook.rs:2093-2104`); a `Rejected` route prints a
  diagnostic and emits nothing (`:1908-1921`); spool entries are profile-tagged
  (`hook_spool.rs` `routed_to`) and drained only with the profile's own token
  (`:1829-1835`, `ProfileTokens`). The native hook already prints the
  session-start handoff to stdout for OpenCode-family agents (e.g. the
  `*_session_start_prints_handoff*` tests).
- **V2 tool payloads need no new native arm.** `opencode2` maps to
  `AgentKind::OpenCode` (`ids.rs:410`); `tool_payload_fields`
  (`capture_policy.rs:371-374`) already reads the V2 `tool`/`callID` shape.
- **The existing native-spawn template.** `postStop` (`:4598-4624`) spawns
  `NATIVE_HOOK` with an argv array (no shell), `windowsHide: true`, the
  profile-free env plus `AI_MEMORY_AUTH_TOKEN`, a 5 s `timeout`, the `unload`
  signal, payload piped on stdin, and a fall-through to `postHook` on spawn
  error. This is the template the new path reuses.

## Design

### One sentence

For the OpenCode plugins (V1 and V2), when `capturePolicy` would drop an event
*because the repository is server-routed*, spawn the native hook for that event
instead of dropping, passing the expected profile name; the native side
re-resolves the profile from the same cwd, refuses anything that is not exactly
that profile, and owns the credential, spool and handoff.

### Why the TS side stays credential-free

The TS layer never learns the profile's token or URL. It passes only the cwd
(inside the payload, as every event already does), the event, and the profile
*name* it read from the marker. The native binary re-derives the route from
cwd and reads the token from `auth-tokens/<name>`. This mirrors `postStop`,
where TS passes payload + `--server-url SERVER` and the binary owns routing.
The install `SERVER`/token passed in the env is the default one; the native
resolver overrides it for a `Profile` route and ignores it, so even a present
default token cannot leak a profile repo to the default server.

### The fail-closed guard: `--expected-profile <name>`

The TS plugin and the native binary each walk the marker tree independently.
Without a cross-check, a divergence (a race that rewrites the marker, a
symlink the two resolve differently, a native walk that finds *no* `server`
key where the TS walk found one) could let the native hook resolve to
`InstallDefault` and deliver a profile repo's data to the install server. That
is the exact #992 failure.

`--expected-profile <name>` closes it. After `resolve_hook_route`, the native
hook requires `HookRoute::Profile(resolved)` with `resolved.name == <name>`.
Anything else (`InstallDefault`, `Rejected`, or a *different* profile name)
prints a localized stderr diagnostic, captures nothing, spools nothing, fetches
no handoff, and writes nothing to stdout (so no handoff is injected). It returns
success (exit 0), the same emit-nothing posture the existing refused-profile
`Rejected` branch takes. Fail-closed means "deliver nothing", and the harness
already cannot distinguish a refusal from an absent handoff. The guard applies
to both capture and the handoff fetch and is additive: without the flag the
native hook behaves exactly as today.

`<name>` is a validated `ProfileName` on the native side (reusing
`server_profiles::ProfileName`), so a malformed value is refused before any
resolution.

### Baking `NATIVE_HOOK` for V2

`NATIVE_HOOK` must be available whenever an OpenCode install could meet a
profile repo, independent of `--capture-assistant`. The plugin is generated per
install, before any repo is known, so the exe path is baked unconditionally
for OpenCode V1 and V2. If the binary is absent at runtime, a server-routed
event falls back to **drop** (today's safe behavior), never to HTTP: the
spawn-error handler for a routed event drops instead of calling `postHook`.

### Event routing

`postHook` gains a routed branch. In pseudocode of the generated TS:

```
async function postHook(event, payload) {
  const scoped = applyMarkerParams(payload);
  const policy = capturePolicy(event, scoped.cwd, ...);
  if (policy.disposition === "drop") {
    // New: a drop *caused by* server routing goes native for V2.
    const profile = policy.serverRoutedProfile;   // name, or undefined
    if (profile && NATIVE_HOOK) return spawnNativeHook(event, scoped, profile);
    return;                                        // every other drop, or no binary
  }
  enqueueHook(event, url, policy.payload);
}
```

`capturePolicy` is extended to report *why* it dropped: when the cause is
`captureServerRouted`, it returns the profile name (`serverRoutedProfile`)
alongside `disposition: "drop"`. Reading the name is a bounded extension of the
existing marker walk (`captureServerRouted` already finds the `server =` line;
it now returns the captured value instead of a boolean). All other drop
reasons (owned-externally, capture exclusions, metadata-only→drop) report no
profile and keep dropping.

`spawnNativeHook` is the `postStop` template generalized to any event:
`["--data-dir", …, "hook", "--agent", AGENT, "--event", <event>,
"--server-url", SERVER, "--capture-mode", CAPTURE_MODE, "--expected-profile",
<name>]` (+ `--project-strategy` when set, + `--capture-assistant` only for the
stop event), payload on stdin, env token, 5 s timeout, `unload` signal.

Because server-routed events go native and all others go HTTP, an event is
delivered by exactly one path, so no deduplication is required.

### Handoff

The `context` hook's `fetchHandoff`, for a server-routed repo, spawns the
native hook for a handoff fetch and reads the handoff text from stdout
(native owns the single claim). The generated `fetchHandoff` gains a routed
branch symmetric to `postHook`: server-routed + `NATIVE_HOOK` → spawn native,
capture stdout, return it for injection into `event.system`; otherwise today's
behavior. The native handoff fetch uses the same `--expected-profile` guard, so
a divergent resolution yields no handoff rather than one from the wrong server.

Claim-once is the native hook's existing contract. The documented limitation
stands and is restated in the user docs: a crash after the native hook claims
the handoff but before the TS layer injects it loses that handoff (the same
window the issue calls out). No new loss window is introduced versus the
existing native session-start path.

### Session lifecycle

`session.created`/`startSession`, `session.deleted`/`endSession`,
`session.moved`, `session.compaction.*` route through `postHook` and inherit
the routed branch unchanged. A cross-profile session move stays unsupported:
the move emits `session-start` with the new cwd, and if old and new resolve to
different profiles the native guard refuses the mismatched one, so the move is
not silently rebased across profiles.

## Security boundary

This changes the enforcing code of **security-boundaries row 11d** (the #992
server-profile routing guard) for one front door: the generated TS
`captureServerRouted`, which stops being drop-only for the OpenCode plugins (V1
and V2) and becomes spawn-native-with-`--expected-profile`. The row and its
adversarial tests are
updated in the same change. The invariant the tests must keep biting on:

- a profile repo's capture reaches only the profile server, with the profile
  token, never the install default. This holds when the binary is missing
  (drop, not HTTP) and when the native walk diverges from the TS walk (guard
  refuses, not fallback);
- `--expected-profile` refuses `InstallDefault`, `Rejected`, and a different
  profile name, for both capture and handoff, and emits nothing on refusal;
- a non-routed repo is unaffected (control).

No token ever appears in an argv (`--expected-profile` is a name, not a
secret; the token is read by the native side from its file). The spawn is
`shell: false` with an argv array and `windowsHide: true`, stdin-piped, and
time- and signal-bounded, as `postStop` already is.

## Phases (each a vertical slice with tests)

1. **Native `--expected-profile`.** Add the flag to `hook.rs`, validate the
   name, enforce the match after `resolve_hook_route` for capture and handoff,
   fail closed (emit nothing). Tests: resolved-profile match captures/spools to
   the profile with its token (control); `InstallDefault`, `Rejected`,
   mismatched-name and a malformed name each emit nothing and inject no handoff.
   These are the `hook.rs` / `server_profiles.rs` integration suites.
2. **`capturePolicy` reports the routed profile name.** Extend
   `captureServerRouted`/`capturePolicy` in `render_shared.rs` to return
   `serverRoutedProfile`; keep every existing `server-routed-*` vector dropping
   with the name reported. Node-runtime evidence test updated.
3. **V2 native spawn for routed events.** Bake `NATIVE_HOOK` unconditionally
   for V2; add the routed branch to `postHook` and `spawnNativeHook`; missing
   binary → drop. Tests assert the generated plugin's spawn argv shape (as
   `:14350` does for stop) and the drop-on-missing-binary path.
4. **Handoff via native.** Routed branch in `fetchHandoff`; native handoff on
   stdout injected once. Test: routed repo injects the profile server's
   handoff; a divergent/absent profile injects nothing.
5. **Docs + boundary.** `docs/marker-file.md` and `docs/install.md` (OpenCode
   V2 + server profiles), the pt-BR mirror, security-boundaries row 11d, the
   CHANGELOG `### Added` entry, and the crash-window limitation note.

## Acceptance tests (issue's matrix, mapped)

- Two disposable repos, each a restricted profile + credential, one server;
  positive capture per project, cross-project read **and** write denied →
  `server_profiles.rs` two-real-servers pattern extended to the V2 spawn.
- Missing / invalid / changed / unauthorized profile fails closed without
  default fallback → phase 1 native tests + phase 3 spawn-missing-binary.
- `private/**` pre/post Read events and sensitive content from public paths →
  reuse the capture-policy privacy vectors through the native path.
- Repos without a named profile keep existing behavior → the control in every
  phase.
- Subprocess exit alone is not proof of storage → tests assert on the server's
  received events and tokens, not on the child exit code.

## Open questions for sign-off

1. **Scope (resolved).** Both OpenCode V1 and V2. The routed-native branch and
   the shared `spawnNativeHook` helper live in the shared capture prelude, and
   `NATIVE_HOOK` is baked unconditionally for both plugins, so neither depends
   on `--capture-assistant` for profile capture.
2. **`--capture-mode` for routed tool events.** The routed spawn passes the
   install's `CAPTURE_MODE`; the native side still applies the profile's own
   marker exclusions. Default: no per-profile capture-mode override (the
   profile's exclusions already govern its own repos).
