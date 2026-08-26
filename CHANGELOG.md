# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- The native client now trusts CAs from the platform trust store. It was built
  with reqwest's `rustls-tls`, which bundles the Mozilla webpki roots and
  ignores the OS store, so a CA the operator had installed locally — Caddy's
  `tls internal`, a corporate MITM appliance, any private PKI — was invisible
  to `ai-memory` even though `curl` and the agent CLIs trusted it. Every HTTPS
  request failed with `invalid peer certificate: UnknownIssuer`, including
  `ai-memory hook`, so following this project's own HTTPS-via-proxy Path 2
  guide left lifecycle capture failing end to end: events spooled locally
  (queuing touches no network) and the drain could never deliver them.
  Switched to `rustls-tls-native-roots`; the runtime image installs
  `ca-certificates`, so the server path keeps a populated store. Reported by
  @alanmatiasdev (#492).

- Made `hook-drain` say what a pass actually did. The drain already returned
  `sent`/`remaining`/`dropped` counts and `run_drain` discarded all three, so
  three passes that mean opposite things — everything delivered, every queued
  event discarded undelivered at the retry cap or the spool TTL, and nothing
  attempted at all because another drainer held the lock — were byte-identical
  on both streams and in the exit code. A drain that silently loses capture was
  therefore indistinguishable in the field from one that works, which is how
  #493 was reported: exit 0, no output, an empty `hook-drain.log`, and no way
  to tell which of the three had happened. A pass that leaves nothing behind
  stays silent, so `hook-drain.log` keeps receiving only warnings and a healthy
  instance never grows it; a pass that leaves events queued or drops them, and
  a pass that found the lock held, now each emit one stderr line. stderr, not
  stdout: the drainer's stdout is contractually empty and the detached helper
  already redirects stderr into the log. This changes no delivery behaviour —
  it only stops the existing behaviour being unfalsifiable. (#493)
- Stopped the scaffolding filter discarding terse prompts. The identifier
  branch added in #484 fired on any single token carrying a digit and a
  hyphen, underscore or bracket, which is the shape of a bare model id and
  equally the shape of `pr-477`, `issue-484` or `commit-a0bc43d`: measured
  against the two populations, it matched 10 of 10 model ids and 7 of 11
  plausible terse references, so a page could lose a real title. The class is
  now excluded at its source instead — `derive_title` skips `SessionStart`
  outright, the only kind whose title `best_title_hint` fills from the
  harness's `model` field — and the branch is gone. That also excludes the
  router's default `title_hint.unwrap_or(kind)`, the literal `session-start`
  written whenever a harness sends neither `model` nor `title`. The trade is
  narrower than a shape rule and stated as such: a model id a user *typed* is
  now kept, because at that point it is what they wrote. Where the prompts
  are unusable a page falls to the next observation title, which the router
  defaults to the observation's own kind, so `Session {id}` is reached only
  by a session carrying nothing else at all — measured over 332 real sessions
  it never was, and 5 of them landed on `tool file` / `tool non-file` against
  327 keeping a real title. `looks_like_scaffolding` is re-exported from
  `ai-memory-core`, so its narrowing is a public-API behaviour change even
  though `derive_title` is its only caller in tree. (#484)

### Docs
- Documented why registering ai-memory through Kimi Code's own
  `kimi mcp add` breaks every model turn: that command writes the plain
  `/mcp` URL with no `?flavor=moonshot`, so Moonshot rejects
  `memory_read_page`'s root-level `anyOf` and fails the request — including
  requests that use no tools, since schemas ship with each one. `kimi mcp
  test` passes regardless because it never sends schemas upstream, which
  makes the server look healthy. Records both escapes: re-run `install-mcp
  --client kimi-code`, or set `strip_root_combinators` server-side to cover
  any strict client that skips the marker. Reported by @LeandroCorsoOrion
  (#474).
- Documented the `os error 4551` build failure on Windows machines with Smart
  App Control / App Control for Business enforced. Cargo compiles each
  crate's `build.rs` into an unsigned executable under `target\debug\build\`
  and those policies block it, so the build dies on `proc-macro2` before
  reaching any ai-memory code and looks like a broken toolchain. Reported by
  @CaioCoelhoChaves (#478).

## [1.32.1] - 2026-08-25

### Fixed
- `memory_forget_sweep`'s tool description claimed "Semantic / procedural /
  pinned pages are exempt" without qualification. That is true of the decay
  pass and false of the TTL pass, which hard-deletes any page whose
  `expires_at` has passed regardless of tier or pin — the check runs three
  lines before the only pin test. Since the tool is admin-gated and
  destructive and its description is the only thing an agent can consult, a
  model asked "is it safe to run, I have pinned pages?" had no way to answer
  anything but yes. Both surfaces — the `tools/list` description and the
  `MEMORY_INSTRUCTIONS` line — now name all three passes and scope the pin
  exemption to the one that honours it. Behaviour is unchanged: an explicit
  expiry remains a more specific instruction than a pin. A `tools/list`
  assertion pins the wording so a fourth pass cannot silently make it stale
  again. Reported by @samirhvbr (#485).
- Hits ranked by the vector stream returned an empty `title` and `snippet`,
  even when the entity or graph stream had also found the page and already
  computed a real descriptor for it. The fusion map is built with
  `or_insert_with` and the vector loop only has `(PageId, PagePath, f32)` to
  insert, so first-writer-wins left the entry empty and nothing downstream
  repaired it. Beyond the empty display, `title` and `snippet` are the
  reranker's candidate text, so an affected page was scored as an empty
  document and could then be truncated out of the results a reranker was
  meant to improve. Fused hits that still lack a title are now hydrated in
  one bounded lookup; hits another stream already described keep that
  stream's snippet, so an FTS excerpt centred on the matched term is not
  downgraded to a generic descriptor. Affects instances with an embedding
  provider configured. Reported by @samirhvbr (#486).
- Session page titles were the first user prompt taken verbatim, so whatever
  the harness put in that payload became the title the page is indexed and
  displayed under — IDE context blocks, a shell prompt echoed into a paste, a
  bare model id. Measured across one live instance: 21 of 330 real session
  pages (6.4%), at a flat rate month over month rather than a decaying legacy
  population. Titles that read as harness scaffolding are now skipped in
  favour of the next real prompt, and a session whose every candidate is
  scaffolding falls back to its own identity rather than a shared literal.
  The predicate tests the *shape* of the text rather than matching a list of
  known offenders, since three classes from one corpus is a sample of that
  operator's harnesses and a blocklist would fail silently on the fourth.
  New writes only — existing titles are unchanged. Reported by @samirhvbr
  (#484).

## [1.32.0] - 2026-08-24

### Added
- Gave session pages a `summary` in their frontmatter, which the retrieval
  layer already preferred over the page body when describing a non-FTS hit.
  #463 made that `COALESCE` prefer a summary, but no write path produced one:
  measured against a live 1.31.1 instance, 0 of 25 substantive pages had the
  field, so every descriptor fell back to the body. All three writers now fill
  it — the zero-LLM `SessionEnd` synthesis, the single-page consolidator, and
  the batch one — because zero-LLM is the documented default and `multi_page`
  defaults to false, so covering either alone would have missed the pages most
  installs actually hold. (#473)

  On the zero-LLM path the summary came from counts the renderer already had
  and was discarding: prompts, completed tool calls per tool, and elapsed time
  ("2 prompts, 34 completed tool calls across Bash, Edit and Read, over 18m").
  That is what a page cannot state about itself — a reader deciding whether to
  open it now sees how much work it holds, not only what it opened with. The
  page is tallied once and the count lent to both the body renderer and the
  summary, and `PostToolUse` stays the only counted kind so a completed call is
  not double-counted.

  On the LLM paths it became an optional field on the consolidator's structured
  output, costing no additional model call since the call already happens, with
  `#[serde(default)]` so stored outputs from earlier runs still deserialise.
  Both system prompts now request it and describe the shape it has to keep; the
  batch prompt previously forbade the key outright.

  A model-supplied summary is validated before it is written, because the
  reader prefers `summary` over the body and then drops headings, metadata
  bullets, list items and title repeats — falling back to echoing its raw
  input when nothing survives. A structurally wrong summary is therefore not
  ignored but reproduced verbatim as the descriptor, displacing usable body
  text with no error anywhere. Anything the reader would discard is dropped at
  the boundary instead, leaving the page on its body-derived descriptor.

  The fallback is unchanged and still serves hand-written pages and everything
  written before this. New writes only: no backfill, no migration.
- Pool (Poolside Agent CLI) is a first-class agent kind. Hook ingestion
  recognizes `agent=pool` (alias `poolside`) and Pool's documented snake_case
  `session_id` / `cwd` / `tool_name` / `tool_input` payload for concrete
  session attribution and tool-family titles, and Pool's five events
  (`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop` —
  verified against Poolside CLI v1.0.16) ship as a `hooks/pool/` bundle.
  `install-hooks --agent pool` stages the scripts and prints a ready-to-paste
  `.poolside/settings.yaml` `hooks:` snippet — Pool's hook config is
  project-scoped YAML at each repo root, so the installer deliberately does
  not write project-local files. Native hook commands enforce `[capture]
  ignore_paths` for Pool's `read`/`edit`/`write`/`remove` file tools, with
  unknown payload shapes degrading to metadata-only capture. Pool has no true
  session-end event, so `ai-memory finalize-session --agent pool` closes the
  session (the same class as Codex and Antigravity CLI); a forward migration
  (V50) rebuilds the sessions constraint and pairing triggers to accept the
  `pool` kind without losing rows. `SessionStart` stdout injection is not
  demonstrated, so the session-start hook never fetches the single-use
  handoff — recover it via MCP `memory_handoff_accept`. No managed
  workstream (`ai-memory run pool`) is claimed: Pool's native session-store
  contract is not demonstrated, per docs/managed-harness-contributions.md.
- `ai-memory status` now reports server-side hook-ingestion counters
  alongside the client spool section: events accepted, accepted-but-dropped
  by capture policy, shed because ingest capacity was exhausted, shed by the
  per-source rate limiter, and how long since the last event reached the
  writer (#428). Together with the spool numbers this distinguishes the three
  states that previously looked alike from the outside — hooks not arriving,
  hooks arriving but shed, and hooks arriving but not landing.

  Deliberately a fixed set of scalars rather than a per-source breakdown:
  keying counters on a client-supplied value would put unbounded,
  network-reachable state behind the very queue that exists to bound it. The
  counters are relaxed atomics, so the hook path pays no lock, and the
  snapshot is content-free by construction — counts and one timestamp, with a
  test asserting the field set so a future change cannot quietly add captured
  text. A newer CLI against an older server renders the rest of `status` and
  omits the section rather than failing to parse.
- `install-hooks --capture-mode allowlist` inverts capture scope: a repository
  with no `.ai-memory.toml` marker emits no lifecycle event at all — prompts,
  tool calls and session boundaries alike — dropped in the hook process before
  anything reaches the local spool or the wire. Previously the marker could
  only narrow what an already-captured repository sent, so forgetting one
  captured *more*; under this mode forgetting one captures less (#446).

  The gate is deliberately independent of the per-event capture policy, which
  runs only for tool events: expressing it as a capture disposition would have
  left prompt text spooling from a repository the CLI reported as opted out.
  The mode is stored once per install rather than baked into each agent's hook
  command, so every agent honours it and a bare `--apply` — including the
  refresh inside `upgrade` — cannot revert it. `--check-capture` now reports
  `capture_mode`, `marker_present` and `admits_capture` so an opt-out can be
  verified without sending anything. Default behaviour is unchanged, and
  `--capture-mode denylist` restores it.
- Documented allowlist mode in the README feature list beside the existing
  per-repository capture exclusions, so the two capture controls are
  discoverable from the same place (#446).
- Corrected the allowlist-mode documentation, which claimed "every agent's
  hook honours it". The gate runs inside the native `ai-memory hook` binary
  before the spool write, so script-based installs — the
  `AI_MEMORY_HOOK_PLATFORM` override, the Docker host wrapper, and
  `setup-agent` snippets — POST directly and never read the mode. That is the
  same boundary `[capture] ignore_paths` already documents. `install-hooks
  --apply` now warns when the install it is writing cannot enforce the mode it
  just stored, so the gap is visible where it matters rather than only in a
  doc (#446).
- `ai-memory status` now reports the capture mode when it is `allowlist`, in
  both the human and `--json` renderings. Without it the new ingest counters
  read identically whether hooks are broken or a repository simply never opted
  in — the exact ambiguity those counters were added to remove (#428, #446).

### Docs
- Documented the order of magnitude reported for lifecycle-hook overhead,
  including the fact that a completed tool call normally pays for both pre- and
  post-tool hooks, the measured native-versus-script figures, and the host
  factors that make them reference data rather than a performance guarantee
  (#446).

## [1.31.1] - 2026-08-23

### Fixed
- `ai-memory upgrade` now verifies that a discovered Compose project actually
  owns the running `ai-memory` container before invoking `docker compose up`.
  A standalone `docker run` install could previously be mistaken for a Compose
  deployment merely because an unrelated `docker-compose.yml` existed in a
  conventional search path; Compose then failed with a container-name conflict
  and the safe standalone recreation-script fallback was skipped. Unowned
  containers now use that existing fallback, preserving their inspected ports,
  mounts, restart policy, command, and operator-set environment. (#469)
- The Windows Docker wrapper's thin-client commands now reach a
  loopback-published server instead of failing with `Connection refused (os
  error 111)`. `ai-memory status`, `search`, `bootstrap` and every other
  thin-client command runs inside a short-lived helper container, where the
  CLI's default `http://127.0.0.1:49374` resolves to that helper rather than
  to the Windows host, so a healthy `docker run -p 127.0.0.1:49374:49374`
  server was unreachable from the wrapper while `curl` from PowerShell worked.
  The wrapper now injects `AI_MEMORY_SERVER_URL=http://host.docker.internal:49374`
  for those commands, matching what the POSIX wrapper has done on macOS since
  (#107) — Docker Desktop gives Linux containers no host networking on either
  platform. `install-mcp`, `install-hooks` and `setup-agent` still render
  `http://127.0.0.1:49374` into host-side agent config, because
  `host.docker.internal` does not resolve on the Windows host, and an explicit
  `AI_MEMORY_SERVER_URL` (homelab or remote server) is still honoured. (#464)
- Page paths that cannot be materialised on every supported platform are now
  refused at write time instead of failing late (#462). `PagePath` accepted
  DOS device names (`CON.md`, `aux.md`), components ending in a dot or space,
  the characters `< > : " | ? *`, and control characters. On native Windows
  some of those failed with a generic HTTP 500, while others were written
  successfully but could not be checkpointed by libgit2 or deleted through the
  normal API — silent partial state, which is worse than a clean rejection.
  The check runs in `Wiki::write_page`, the single funnel every page creation
  passes through, and returns an error naming the offending component and the
  reason. It is deliberately **not** in `PagePath::new`: persisted rows are
  reconstructed through that constructor on every read, so tightening it would
  make an already-stored non-portable page unreadable and break the whole
  listing it appears in. The rule is identical on every platform, so a wiki
  authored on Linux stays usable on Windows.
- Made non-FTS search hits describe the page instead of repeating its
  heading. Only the FTS5 path built a real excerpt (`snippet(pages_fts, ...)`,
  centred on the matched terms); the vector, entity-match, graph-neighbour and
  recency paths had no matched term to centre on and returned
  `substr(body, 1, 240)` — on a compiled page that is the `# Title` line plus
  the following structural heading, which duplicates the `title` field already
  present on the hit and gives an agent nothing to decide a follow-up
  `memory_read_page` on. Measured against a live instance over 100 real pages
  (55 of them substantive, the rest near-empty session stubs), 53% of
  substantive descriptors opened by repeating the title verbatim, and the
  median page spent its 240-character budget on 35 characters of prose. Those
  six queries now prefer the page's own frontmatter `summary`, and otherwise
  fill the budget from the page's own text, skipping structural lines,
  `- **key:** value` metadata bullets, and any line that merely repeats the
  title. On the same sample title repetition drops to 0%, and 71% of session
  pages land on the prompts after the first — the part the title does not
  already show. The FTS path is unchanged, response shape is unchanged, and no
  schema migration is involved.

## [1.31.0] - 2026-08-22

### Fixed
- Auto-improve no longer rejects every proposal from a model that spells the
  single supported operation differently (#458). `operation` was a free-form
  string validated by exact match against `create_or_update`, while the
  schema carried no constraint and the neighbouring field description — "path
  that would be created or updated" — actively invited `"create"`. Two local
  models took the invitation for 6/6 candidates, so with
  `require_approval = false` the learning loop completed having silently done
  nothing, and the rejection buffer then fed `unsupported_operation` rows back
  into later reviewer prompts as noise. The schema now advertises a
  single-value enum, so providers with constrained decoding cannot emit
  anything else, and validation normalises the unambiguous spellings
  (`create`, `update`, `upsert`, `create/update`, …) for providers without it.
  Operations the pipeline genuinely cannot perform — `delete`, `rename`,
  `move` — still fail with the same reason as before.

### Added
- The `gemini` LLM provider now respects the `LLM_BASE_URL` configuration value.
  Previously, the base URL was hardcoded to `generativelanguage.googleapis.com`.
  This allows using self-hosted, local Gemini-compatible models (like Gemma 3 hosted via Litert) that expect the native `/v1beta/models/...:generateContent` payload shape.

## [1.30.0] - 2026-08-21

### Fixed
- Documented the `ai-memory status` hook-spool section in
  `docs/deploy.md`. It shipped with no operator-facing description, in the
  one document whose troubleshooting list is where someone looks when
  capture seems delayed. (#428)
- The Windows packaging test for the PowerShell wrapper now runs it with
  `-ExecutionPolicy Bypass`. `-File` loads a script from disk and execution
  policy governs script files, so on a machine at the Windows client default
  of `Restricted` the unsigned wrapper was refused and the test failed for a
  reason unrelated to what it asserts. Test-only; no shipped behaviour
  changes. (#449)

### Added
- `install-hooks --agent claude-code --no-capture-prompts` can now omit
  ai-memory's `UserPromptSubmit` hook, preventing prompt text from entering the
  local spool or wire while leaving session, tool, compaction, and handoff
  capture active. Re-applying or upgrading preserves the opt-out, third-party
  hooks under the same event survive, and `--capture-prompts` explicitly
  enables prompt capture again. Other agents reject both flags because some
  depend on their prompt hook to inject handoff context. (#446)
- New `gemini_safe_schemas` config flag (env `AI_MEMORY_GEMINI_SAFE_SCHEMAS`, or
  `gemini_safe_schemas = true` in config.toml) and a matching `?flavor=gemini`
  MCP URL marker (alias `?flavor=vertex`) serve tool input schemas in the subset
  Google's `Schema` accepts. `schemars` renders every optional tool argument as a
  nullable union (`"type": ["integer", "null"]`), but Vertex/Gemini
  `functionDeclaration.parameters` takes a single `type` and treats `any_of` as
  exclusive with every sibling key — so a client that forwards our schema
  verbatim produced `any_of` with `description` beside it and Vertex failed the
  whole session at `tools/list` with "specified other fields alongside any_of".
  The dialect collapses those unions to a single `type` plus `nullable: true`,
  the same normalization Gemini CLI performs client-side, which is why Gemini CLI
  and Antigravity CLI never hit this and need no marker; pass-through clients such
  as OpenCode on a Vertex model did. It implies `strip_root_combinators`, and
  runtime argument validation is unchanged in every dialect. The key is
  documented in the generated `config.default.toml`, and `docs/mcp-install.md`
  now collects all three schema dialects in one table. (#450)
- Windows tests moved to their own workflow
  (`.github/workflows/windows.yml`), running on every push to `main`,
  nightly, on demand, and on any PR labelled `windows`. They were the
  slowest job in `ci.yml` by a wide margin (~1000s against ~250s for the
  same tests on Linux) while carrying `continue-on-error`, so every pull
  request waited roughly seventeen minutes for a job that gated nothing
  when all gating jobs finished in about eight. Coverage is now stronger,
  not weaker: `continue-on-error` is gone, so a genuine Windows break shows
  as a failed run on `main` instead of a yellow mark on a PR, and the
  nightly run catches toolchain or dependency drift that no code change
  would trigger.
- `ai-memory status` now reports local hook-spool health: pending event
  count, age of the oldest queued event, and total failed-delivery attempts.
  This makes "memory capture is delayed, not broken" visible to an operator
  without digging through logs: a non-empty spool or an aging oldest event
  means hook events are queued locally instead of reaching the server. The
  spool is client-side, so the section reflects the local data-dir even when
  the server is remote. The JSON form gains a matching `spool` object
  (`pending`, `oldest_age_ms`, `retries_total`). Only files matching the
  spool's own naming convention are counted, so a foreign `.json` in that
  directory cannot inflate the pending count or be reported as an
  epoch-aged entry. The section is also reported when the server cannot be
  reached — the spool exists to buffer events while the server is down, so
  that is precisely when its depth is worth seeing; it goes to stderr, so
  `--json` consumers still get a single object on stdout. (#428)

## [1.29.0] - 2026-08-19

### Fixed
- Corrected the Pi/OMP extension filenames in `README.md`, `docs/install.md`,
  and `docs/mcp-install.md`, which still described the pre-rename
  `ai-memory.ts` after the agent-distinct `ai-memory-pi.ts` /
  `ai-memory-omp.ts` split. Following those instructions by hand created the
  very duplicate-load the split exists to prevent, since each agent loads
  every `*.ts` in its extensions directory. Documented `--profile` /
  `OMP_PROFILE` and the `PI_CODING_AGENT_DIR` precedence alongside them.
- The lint `stale` threshold is now derived from the operator's `[decay]
  lambda` instead of a hard-coded 30 days (#426). The rule only fires on
  episodic pages with zero accesses, and for those the decay score reduces
  exactly to `salience * exp(-lambda * age)` — the reinforcement term carries
  `ln(1 + access_count)`, which is zero — so the lint was already measuring
  decay, just against a constant rather than against `lambda`. Slowing decay
  made the two diverge: at `lambda = 0.008` real eviction lands near day 201
  while the lint still called a page stale on day 31, and because a report
  page is written whenever any finding exists, one page nobody intended to
  read produced a new `_lint/<date>.md` every day, forever. The threshold is
  now `0.6 / lambda`, which is **exactly 30 days at the default
  `lambda = 0.02`** — unchanged for anyone who never tuned decay — and 75
  days at `0.008`. An invalid or non-positive lambda falls back to the
  historical constant rather than silencing the rule.
- `bin/deploy` now refuses to push a single-architecture build over a tag that
  already resolves to a multi-architecture manifest (#427). It builds for the
  architecture of the machine it runs on, so deploying a homelab from an x86
  workstation to `IMAGE=akitaonrails/ai-memory:latest` replaced the
  amd64+arm64 manifest CI had published with an amd64-only image, and every
  arm64 host pulling `:latest` failed with `exec format error`. The shipped
  `bin/deploy.env.example` also defaulted `IMAGE` to `:latest`, so following
  the documented setup led straight into it; it now defaults to a private
  `:homelab` tag and explains that release tags are published by CI only.
- `install-hooks` now writes agent-distinct extension filenames
  (`ai-memory-pi.ts`, `ai-memory-omp.ts`) instead of both agents sharing
  `ai-memory.ts`, so a Pi install and an OMP install no longer overwrite each
  other when `PI_CODING_AGENT_DIR` points them at one home. A superseded
  `ai-memory.ts` is removed only when it carries this tool's generated marker
  *for that same agent* — a file you wrote yourself, or the other agent's, is
  left alone and reported. OMP profiles are supported via `--profile` or
  `OMP_PROFILE`; `PI_CODING_AGENT_DIR` takes precedence over a profile, since
  it names the agent directory outright.

  Note distinct filenames stop the overwrite but do **not** make a shared
  directory safe: each agent loads every `*.ts` it finds there, so installing
  both into one home captures every event twice, once per agent identity.
  That collision is upstream — both agents read the same environment
  variable — so `install-hooks` now warns when it detects one and points at
  the two ways out. (#421)
- `install-hooks --agent pi`, `--agent omp`, and `uninstall` now honor
  `PI_CODING_AGENT_DIR`, instead of always writing to `~/.pi/agent/extensions/`
  and `~/.omp/agent/extensions/`. Both agents relocate their whole agent
  config home (`~/.pi/agent` / `~/.omp/agent`) through that variable, so on an
  install with it set the extensions landed where the agent never loads them:
  the install reported success and capture silently did nothing. ai-memory
  already honored the variable when resolving Pi/OMP transcripts, so the two
  halves of one install disagreed about where that home was. Installs without
  `PI_CODING_AGENT_DIR` set are unaffected. The manual (non-`--apply`)
  instructions now name the resolved path too, instead of always printing the
  `~/.pi/agent` / `~/.omp/agent` default. (#411)

### Added
- CI now rejects a CHANGELOG version section that repeats a `### ` heading
  (`scripts/check-changelog-sections.sh`). `bin/release` copies
  `[Unreleased]` into the new version verbatim, so a duplicated heading ships
  release notes with the entries split across two identical headings — valid
  Markdown, invisible in review. The same defect was found in four already
  released sections (1.19.0, 1.13.0, 1.4.0, 1.1.1) and folded in the same
  commit; no entry text was changed.
- New `llm_timeout_secs` config key (env `AI_MEMORY_LLM_TIMEOUT_SECS`, or
  `llm_timeout_secs = 900` in config.toml) overrides the per-request timeout
  every chat provider applies to its HTTP calls — previously hardcoded at
  300s per provider. Slow hosted gateways (observed with free aggregator
  tiers whose long completions exceed five minutes) failed every request with
  `http: error sending request` once generation crossed the ceiling, so LLM
  consolidation exhausted its retries and left heuristic pages behind; now
  operators raise the bound at startup instead. The Copilot token exchange
  is bounded by the same value, and the 300s default is unchanged ([#435]).
- Added a `flake.nix` so NixOS and Nix users can build and run ai-memory
  without the Rust toolchain or Docker: `nix build`, `nix run . --
  --version`, or `nix develop` for a dev shell. The build is self-contained
  (SQLite bundled, libgit2 vendored, rustls with webpki-roots — no OpenSSL,
  no system-library hunting). The flake skips the packaging test suite
  because those tests exercise the Docker-wrapper shell script and need
  `docker`/`podman` on PATH; the rest of the workspace tests can be run via
  `nix develop -c cargo test --workspace`. Flake inputs are pinned to
  explicit revisions rather than floating branches, so the build is
  reproducible, and a `nix` CI job builds the flake and runs the resulting
  binary whenever a Nix build input changes plus weekly, so the packaging
  cannot rot unnoticed. ([#405])
- New `strip_root_combinators` config flag (env `AI_MEMORY_STRIP_ROOT_COMBINATORS`,
  or `strip_root_combinators = true` in config.toml) strips root-level
  `anyOf`/`oneOf`/`allOf` from MCP tool input schemas on every `tools/list`.
  Generic MCP clients such as OpenCode and Cursor never send the `?flavor=`
  marker, yet forward schemas verbatim to strict upstreams (Moonshot, Bedrock)
  that reject root combinators with a 400 — this gives operators behind such an
  upstream a mode-independent opt-in. Runtime "exactly one of" validation is
  unchanged ([#412]). The key is documented in the generated
  `config.default.toml` so it is discoverable without reading the source.

### Changed
- Changed the default model for the `gemini` provider from `gemini-2.5-flash`
  to `gemini-3.5-flash`. Google has scheduled the 2.5 Flash family for
  retirement — the Gemini API deprecation table lists `gemini-2.5-flash-lite`
  for **October 16, 2026** (Vertex AI and the Agent Platform list October 20;
  ai-memory talks to the Gemini API, so the earlier date is the one that
  applies). The thinking-budget workaround still covers the legacy models.
  Note it is deliberately **not** applied to `gemini-3.5-flash-lite`, which
  rejects `thinkingConfig` outright with HTTP 400 — unlike
  `gemini-2.5-flash-lite`, which accepts it — so that model omits the field
  and the e2e smoke test keeps `gemini-2.5-flash-lite` as its second
  variant. (#423)
- Documented OrcaRouter through the existing `openai-compat` provider instead
  of adding a redundant provider type, including the endpoint, model, and API
  key mapping needed for deployment. (#410)

### Improved
- The `open_questions` field in automatic SessionEnd handoffs now uses
  multi-signal heuristics instead of blindly copying the last user prompt.
  Trailing acknowledgments ("ok", "thanks", "好的") are filtered; a
  question-mark-terminated final prompt is tagged as an unresolved question;
  file-tool activity without a subsequent Stop produces an advisory to check
  the working tree; and a mid-task exit (Stop without SessionEnd) is flagged
  so the receiver knows the previous session did not finish cleanly. (#425)
- Observation bodies that exceed the 16 KiB durable ceiling are now
  truncated with a **head-tail** strategy instead of head-only: the first
  and last ~8 KiB are preserved with a `[truncated N bytes]` marker between
  them. A 50 KB tool output previously lost its tail entirely, so the LLM
  consolidator saw an incomplete picture; the new truncation keeps the
  outcome and any trailing error context visible without increasing the
  storage budget.

## [1.28.1] - 2026-08-18

### Security
- Updated `h2` 0.4.14 → 0.4.16 for [RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258),
  an unbounded-empty-DATA-frame denial of service. `h2` is a transitive
  dependency of the HTTP stack ai-memory's own server runs on (axum/hyper), so
  a `--transport http` deployment was reachable by this, not only outbound
  client use. Lockfile-only change.

### Added
- The built-in sanitizer now redacts seven further credential shapes, completing
  three vendor families it already covered only in part. **GitHub:** every
  token prefix — `gho_` (OAuth, the form `gh auth login` writes to disk),
  `ghu_`, `ghs_`, `ghr_` — where previously only `ghp_` and `github_pat_`
  were caught. **AWS:** `ASIA…` STS temporary key ids alongside `AKIA…`.
  **Stripe:** `rk_live_` restricted keys alongside `sk_live_`. Plus three
  providers with no prior coverage: **Google OAuth refresh tokens** (`1//…`,
  longer-lived than the `AIza…` keys already handled — they mint access
  tokens until revoked), **Meta / Facebook Graph access tokens** (`EAA…`,
  ad-account, page and business-management scope), **Telegram bot
  tokens** (`<bot-id>:…`, covering both the `AA…` form every issued token uses
  and the shape Telegram's own docs publish), and **GoHighLevel Private
  Integration Tokens**
  (`pit-…`, which do not expire until manually revoked). These run in
  `BUILTIN_PATTERN_STRS`, so unlike
  operator `[sanitize].extra_patterns` they also scrub client-side, before an
  excerpt reaches the local spool or the wire.
  The AWS rule is anchored to the published twenty-character format rather
  than an open tail, because `ASIA` is also an English word and an open
  tail redacted ordinary uppercase text such as `ASIAPACIFICREGION`.

  Behaviour change: text matching these shapes is now replaced with
  `[REDACTED]` where it previously persisted verbatim. An operator who
  *wants* one of them kept visible (for example a public bot id) can add it
  to `[sanitize].allowlist`.

## [1.28.0] - 2026-08-17

### Fixed
- Docker containers no longer refuse to start unauthenticated, which had left
  every container from the README Quick start crash-looping since v1.27.0
  (#407). The v1.27.0 bind guard reads a non-loopback bind as evidence of
  network exposure and refuses without a token. That inference holds on a
  host, but not inside a container: publishing a port with `-p` *requires*
  binding `0.0.0.0` in the namespace, and whether that port reaches the
  network is decided by the host-side publish spec — `-p 127.0.0.1:49374:49374`
  versus `-p 0.0.0.0:49374:49374` — which the process cannot observe. The
  documented Quick start passes no token and published to loopback, so it was
  safe and refused anyway; with the documented `--restart unless-stopped` that
  became a restart loop. Containers now log a loud warning naming the publish
  spec as the thing to check, instead of refusing. **The host rule is
  unchanged** — an unauthenticated non-loopback bind outside a container is
  still refused. Containers are detected via `/.dockerenv` (Docker),
  `/run/.containerenv` (Podman), or `AI_MEMORY_IN_CONTAINER`, which the
  official image now sets.

  Note the `Host` allowlist does not substitute for a token here: it defends
  against DNS rebinding, where a *browser* sets the header. A client that can
  route to the port sets `Host` freely. If you publish ai-memory beyond
  loopback, set `AI_MEMORY_AUTH_TOKEN`.
- `ai-memory upgrade` no longer tells non-compose Docker users to delete their
  container and rebuild the `docker run` from memory (#407). It now reconstructs
  that container's own stop/remove/run — name, restart policy, published ports,
  mounts, operator-set environment, and an overridden command — and writes it to
  `${XDG_CACHE_HOME:-~/.cache}/ai-memory/recreate-ai-memory.sh` for review before
  you run it. The script is written mode 0600 and never echoed, because a real
  install's environment carries provider API keys and `AI_MEMORY_AUTH_TOKEN`.
  Environment already baked into the image is deliberately omitted, so the new
  image's own defaults are not frozen to the old ones. Compose-based installs
  are unaffected and still upgrade via `docker compose up -d`.
- `install-hooks --agent codex` and `uninstall` now honor `CODEX_HOME`, instead
  of always writing to `~/.codex/hooks.json`. Codex loads hooks from its
  configured home, so on an install with `CODEX_HOME` set the hooks landed
  where Codex never reads them: the install reported success and capture
  silently did nothing. ai-memory already honored the variable when resolving
  Codex transcripts, so the two halves of one install disagreed about where
  that home was. Installs without `CODEX_HOME` set are unaffected.

### Added
- Added `ai-memory move-session <session-id> --to <project> [--to-workspace]
  [--pages move|regenerate] [--confirm] [--force] [--create]`, its batch form
  `--from-project <project> [--from-workspace]`, and `POST /admin/move-session`
  to move one session (or every session touching a project) to another
  project, in the same or another workspace. One transaction per session
  re-stamps the `sessions` row, its `observations` (wherever they landed), the
  `handoffs` it produced, its consolidation jobs, auto-improve runs and
  scheduler claim, and its `sessions/<id>.md` page: `--pages move` (default)
  carries every version and
  the file along (409 when the destination already has that page), `--pages
  regenerate` retires the source page (clearing the session's
  `summary_page_id`) and removes its file so the next consolidation rewrites
  it in the destination. Without `--confirm` the server
  runs the same transaction and rolls it back, so the summary is an exact dry
  run and the CLI prints the command to apply (with `--create` the dry run
  reports `would_create_project` and creates nothing). An open session, a pending or
  running consolidation job, or (batch) the active source project refuse with
  409 unless `--force`. `sessions.cwd` stays as recorded (the response warns
  when its basename is not the destination), and `auto_improve_proposals`,
  `entities` and `page_feedback` are not re-stamped. A session already rooted
  in the destination is re-homed instead of refused: its row stays
  (`session_moved: false`, open-session guard not applied) and only its rows
  still lying in other scopes are gathered (`page: already in destination`
  when its page needs nothing), and the batch form enumerates sessions with a `sessions` row in
  the source OR observations stamped into it, so a phantom project holding
  only observations of sessions rooted elsewhere can be emptied. New
  admission op `move_session`. The dry run and the `/admin/move-session`
  response name every scope the move would drain (`source_scopes`), because
  gathering by session id can empty a project the caller never named and
  moving back does not restore the original split. (#402)
- Added a read path for one session's raw hook observations, before any
  consolidation: the MCP tool `memory_read_session_observations` and the
  `/api/v1` routes `GET .../projects/{project}/sessions` and
  `GET .../sessions/{session_id}/observations`. Both return only the rows that
  landed in the resolved `(workspace, project)`, report `elided_other_scope`
  for a session that crossed repositories, and apply the same owner filter as
  handoffs; a session id from another project or operator reads as not found.
  Pagination is `limit`/`offset` with `total` (default 50, max 200; the
  session list defaults to 20, max 100), `order` is `asc` or `desc`, `kinds`
  and a full-text query narrow the rows, and `body_max_chars` (default 4000,
  `200..=16384`) caps each body with a visible truncation marker. The MCP tool
  reads the latest completed visible session when `session_id` is omitted
  (#401).

## [1.27.0] - 2026-08-16

### Added
- Added `[routing] mid_session` to choose how a mid-session event is attributed
  once the agent's cwd has moved. `follow-cwd` (default) keeps the historical
  per-event resolution; `sticky` keeps the session's project wherever the agent
  wanders, closing the cross-repo `cd` case that split one session's raw record
  across two projects. Lifecycle hooks now tag each `project` override with its
  provenance (`project_src=marker` or `project_src=repo-root`), so `sticky`
  overrules a host-derived repo name while a `.ai-memory.toml` marker still
  wins in both modes. Clients older than this release send no provenance and
  keep their overrides authoritative (#394).
- Added `[auth].secure_cookie` for HTTPS reverse-proxy deployments to mark
  `/web` browser authentication cookies `Secure` while preserving plain-HTTP
  loopback compatibility by default (#396).

### Changed
- Unauthenticated HTTP binds beyond loopback now fail closed before accepting
  requests. Intentional insecure LAN use requires `--allow-insecure-no-auth`;
  authenticated non-loopback HTTP remains available with its TLS warning
  (#396).
- `/api/v1` internal failures now return the fixed body
  `{"error":"internal server error"}` instead of the source error chain, which
  could expose data-directory paths and configuration to a browser. The
  detailed cause is logged server-side (#396).
- The `/web` browser authentication cookie is now `SameSite=Strict` rather than
  `SameSite=Lax`, so it no longer rides top-level cross-site navigations into
  the UI. Existing sessions stay valid (#396).

### Fixed
- Mid-session hook events that resolve to a different project than their
  session — the ordinary result of an agent `cd`-ing into another checkout —
  are recorded again instead of being dropped as a session-UUID collision.
  Owner and agent still identify a session and a mismatch there remains
  terminal; a `SessionEnd` naming a foreign scope is still dropped without
  ending the session (#396).
- Hook session UUIDs now reject cross-owner reuse atomically before ingest-key,
  observation, summary, handoff, or end-state mutation; explicit root
  `finalize-session --all-owners` recovery remains available (#396).
- Newly created data directories, configuration files, SQLite databases,
  managed-workstream segments, and downloaded backups now receive owner-only
  Unix permissions before sensitive content is written, independent of the
  ambient umask. Existing installations are left unchanged; Windows relies on
  filesystem ACLs (#396).
- Under the `repo-root` project strategy, mid-session events whose cwd sits
  outside any git repository and any `.ai-memory.toml` marker (agent scratch
  directories, `/tmp`, data folders) now inherit the session's project instead
  of minting phantom basename projects (`scratchpad`, `data`, `tmp`, ...). The
  host-side hook resolves the repository root itself and sends
  `project=<root name>`, so a missing override already proves the cwd is
  unresolvable; session-sticky attribution now accepts these events even
  outside the session's cwd subtree, with the broad-anchor guards unchanged.
  Deliberate rescopes keep working: git checkouts and markers arrive as
  explicit overrides and never reach stickiness, and the default `basename`
  strategy is untouched (#394).

## [1.26.1] - 2026-08-14

### Fixed
- Forget-sweep decay now removes the authoritative Markdown file while
  conditionally tombstoning the selected page, preventing reconciliation from
  resurrecting evicted content. Aged cleanup recognizes rewritten heads,
  deletes their complete `supersedes` ancestry, ignores lifetime access counts,
  and preserves a newer page recreated at the same path. The corrected scoped
  tombstone index is added by a new migration rather than rewriting history
  (#391).
- The `pages_fts_rows` and `observations_fts_rows` status counters now count
  indexed documents instead of content-table rows. Both FTS tables use external
  content, so `SELECT COUNT(*)` against them was answered from `pages` /
  `observations` and the `fts: N/M` health pair could never diverge, no matter
  how far the index had drifted (#392).

## [1.26.0] - 2026-08-12

### Added
- Added MCP-only Swival CLI support. `install-mcp --client swival --apply`
  merges ai-memory's native HTTP entry into the project-root
  `.swival/mcp.json`, and `uninstall` removes only the matching ai-memory
  entry while preserving sibling servers. Lifecycle capture and managed
  workstreams remain unsupported because Swival's callback contract does not
  expose a stable session identifier (#385).

### Fixed
- Lifecycle-only sessions containing only `SessionStart` / `SessionEnd` now
  close without generating an empty session page, fallback handoff, or LLM
  consolidation job. Startup handoff claims are bound to the native receiver
  session when clients expose one; if that receiver exits without substantive
  work, the same transaction that ends it returns the accepted handoff to the
  open pool. Tool-bearing zero-prompt sessions remain substantive. Shipped
  Claude Code, Codex, OpenCode, Command Code, Kiro, Antigravity, Devin, Cursor,
  Gemini CLI, and Kimi Code delivery paths forward their available session ids
  so an empty receiver cannot permanently consume real work (#386).
- The `bin/ai-memory` wrapper now detects rootless mode and SELinux under
  podman, so host-file commands stop failing with `Permission denied (os error
  13)` on podman-based distros. Both the `-u 0:0` remap and `--security-opt
  label=disable` were decided from `docker info --format
  '{{.SecurityOptions}}'`, a Docker-only field that podman cannot evaluate;
  the swallowed error left both gates off exactly where they were needed.
  Podman's `.Host.Security.*` keys are consulted when that probe comes back
  empty. `bootstrap` is now covered as well: it only reads host files, but an
  unmapped UID blocks reads just as hard, and it degraded silently to "no
  `.git` found" before dying. Restore archives, explicit config paths, and
  commands using a host-backed `AI_MEMORY_DATA_DIR` now receive the same
  host-file treatment (#388).

## [1.25.0] - 2026-08-07

### Added
- Added version-aware Kiro CLI v3 managed workstreams. `ai-memory run kiro
  --v3` reads the authenticated 2.16.2 nested `session.json` / `messages.jsonl`
  store through a visible-event allowlist, persists the incompatible engine
  flavor in its cursor, resumes with exact `--v3 --resume-id`, and joins bare
  automatic selection alongside v2 without cross-resuming either store. Plain
  returning Kiro launches recover the linked engine transparently; v3 wrapper
  `--yolo` adds no flag because Kiro replaced `--trust-all-tools` with
  `permissions.yaml`. A targeted compatibility path also handles Kiro 2.16.2
  writing v3 sessions to default `~/.kiro` despite custom `KIRO_HOME`: only a
  resume proven to live in that fallback drops the override for its child
  process. The optional deterministic acceptance runner now covers fresh,
  resume, and import round trips for both engines. The server automatic-harness
  validator now accepts the Kiro candidate pool advertised by the client,
  preventing bare `ai-memory run` from rejecting a discovered Kiro session
  before launch (#356).
- Added first-party Command Code MCP and stable lifecycle-hook support.
  `install-mcp --client command-code` merges the documented user-scope HTTP
  entry; `install-hooks --agent command-code` preserves existing settings and
  registers only `SessionStart`, `PreToolUse`, `PostToolUse`, and `Stop` with
  native session attribution, capture exclusions, and startup handoff
  injection. `ai-memory run command-code` now adds v3 checkout-scoped
  transcript discovery, exact `--session <uuid>` resume, automatic harness
  selection, native `--yolo` translation, and a visible-event allowlist that
  retains branch parent ids and summaries while excluding hidden reasoning,
  images, custom/Mod records, and provider metadata. Deterministic acceptance
  covers fresh, resume, and incremental-import round trips. Experimental unsandboxed Mods remain
  excluded, and the turn-only Stop boundary is documented with the manual
  finalizer workflow (#373).
- `ai-memory finalize-session --session-id <uuid>` targets exactly one open
  session instead of "the latest open one for this agent+scope". Agents with
  no true SessionEnd hook (Kiro CLI, Codex, Antigravity CLI) rely on
  `finalize-session` to synthesize the summary+handoff, and the
  newest-first default breaks down with several concurrent sessions for the
  same agent in one project — e.g. multiple terminal tabs each running Kiro
  CLI against the same repo — where it can close out a still-active session
  instead of the one that actually finished. Backed by a new optional
  `session_id` filter on `GET /admin/open-sessions`, composed with (not
  bypassing) the existing owner filter: a session id belonging to another
  operator is still unreachable without `--all-owners`, same as the default
  query ([#374]).

### Fixed
- Kimi Code 0.34.0 managed runs now discover checkout-local sessions from the
  current `state.json` `cwd` field as well as the legacy `workDir` alias. The
  parser rejects conflicting aliases and persisted ids that disagree with the
  session directory, and deterministic acceptance now exercises the current
  state schema (#382).
- Prevented delayed post-tool and shutdown hook tails from redirecting the
  shared active-project fallback after work moved to another project. Only
  session starts, user prompts, and pre-tool events now advance shared or
  identity-only fallbacks; other events refresh exact session mappings, and
  dropped-subagent preflight no longer publishes scope (#372).
- The Anthropic provider stopped sending `temperature` to Claude 4.7 and later
  models, including the Claude 5 families, and to Claude Mythos Preview. Those
  models reject non-default sampling parameters, which made `bootstrap`,
  consolidation, `lint`, and auto-improvement fail with an upstream 400. Both
  `anthropic` and `anthropic-oauth` now omit the field for affected models and
  preserve it elsewhere. `llm-test` also sends a representative 0.2 value
  before provider normalization, so it exercises the same compatibility path
  as the real pipeline (#377).
- Both wrappers (`bin/ai-memory` and `bin/ai-memory.ps1`) now forward
  `ANTHROPIC_OAUTH_TOKEN` and `CLAUDE_CODE_OAUTH_TOKEN` to the helper
  container. Their API-key counterparts were already in the passthrough list,
  so subscription-token setups (`claude setup-token`,
  `AI_MEMORY_LLM_PROVIDER=anthropic-oauth`) silently lost their credential at
  the container boundary. The visible symptom is that the retry `status`
  itself recommends — `llm-test --provider anthropic-oauth` — fails with a
  missing-token error, because `llm-test` runs client-side in the helper
  rather than on the server. The PowerShell wrapper also trims trailing path
  separators as individual characters, so Windows invocations reach Docker
  instead of failing during path normalization (#379).

## [1.24.0] - 2026-08-04

### Added
- MCP-only clients are now visible in the operator's traffic picture.
  Every MCP tool call is counted against its caller — the sanitized
  `clientInfo.name` from the initialize handshake when the HTTP
  transport runs stateful (`--http-stateful`) or over stdio, else the
  `X-Memory-Actor-Agent` overlay an ingress proxy asserts, else
  `unknown` — split into reads and writes and bucketed per UTC day in a
  new `client_activity` table (V46). A new multi-user root-gated
  `GET /admin/activity/by-client?since_days=N` reports the aggregate,
  volume-descending with a name tiebreak (`0`/absent = whole history).
  Counts buffer in memory and flush from one background task on a one-minute
  interval, even when the server becomes quiet; failed batches remain bounded
  in memory and retry once per interval. Process exit can still lose the
  current interval. Each UTC day retains at most 128 distinct names and folds
  additional names into `other`, preventing untrusted client names from
  causing traffic-proportional row growth. Unknown future tools count as
  writes, so an unclassified tool surfaces as suspicious growth instead of
  hiding among reads. This complements
  `/admin/sessions/by-agent`: hook-driven agents open sessions,
  MCP-only clients (VS Code Copilot, Claude Desktop, scripts) never do,
  and until now left no trace at all. (#366)
- Added explicit Kiro CLI v2 managed-workstream launches through `ai-memory run
  kiro` / `kiro-cli`, including native `--resume-id` reuse, `$KIRO_HOME`
  discovery, v2 `--yolo` translation, read-only visible-event import, and
  checkout-scoped UUID/metadata validation. Non-v2 engines pass through
  without session injection. Kiro remains outside bare automatic selection
  until a logged-in current-format acceptance run is available, and v3 managed
  sessions remain unsupported (#356).
- Added verified Kiro CLI lifecycle hooks for both incompatible engines. The
  existing `kiro` / `kiro-cli` target keeps v2's camelCase hooks in agent
  configs; the explicit `kiro-cli-v3` target atomically merges Kiro's
  standalone `v1` registration under `$KIRO_HOME/hooks`. Both preserve
  unrelated entries and project strategies, infer remote MCP connectivity,
  inject startup handoffs, enforce capture exclusions for documented file-tool
  payloads, and uninstall only proven ai-memory entries (#355).
- Added `[consolidation] max_input_tokens` and `max_output_tokens`
  (`AI_MEMORY_CONSOLIDATION__MAX_INPUT_TOKENS` /
  `AI_MEMORY_CONSOLIDATION__MAX_OUTPUT_TOKENS`) for provider-specific context
  limits. Input sizing now accounts for the rendered system/user messages,
  bounded current-page or slot context, structured-output schema, and provider
  envelope reserve instead of budgeting only the observation dump. Unsupported
  minimums are rejected at startup (#369).

### Fixed
- Mixed-case and trailing-period usernames now use deterministic hashed
  per-operator slot namespaces, preventing distinct identities from sharing one
  physical directory on case-insensitive filesystems (#364).
- Consolidation prompts no longer ignore system, schema, instructions, and
  dynamic slot/current-page overhead when allocating observations. The
  provider-neutral estimate is deliberately conservative and documents the
  remaining tokenizer variance instead of claiming an exact token ceiling
  (#369).
- A failing LLM no longer costs a session its PreCompact/PostCompaction
  checkpoint. Provider failures (context overflow, rate limit, outage) and
  unmappable structured output now degrade to the rule-based checkpoint a
  zero-LLM install writes, so configuring a provider can no longer be worse
  than leaving it unset. The fallback keeps the `consolidate` admission event;
  admission rejections, wiki/store failures, and unresolvable sessions still
  fail closed (#369).
- Native-session checkout matching now fails closed when either path cannot be
  canonicalized, instead of treating two missing or inaccessible paths as the
  same checkout during managed resume or adoption (#356).

## [1.23.0] - 2026-08-03

### Changed
- Documented provider-neutral coexistence with structural code-intelligence
  tools: use ai-memory for historical intent and continuity, use the current
  checkout or structural provider for live symbols and impact analysis, verify
  historical structural claims before acting, and keep source, builds, tests,
  and observed runtime behavior authoritative. No provider coupling, automatic
  querying, or persisted structural-evidence schema was added (#353).

### Fixed
- Antigravity CLI's generated native `PreToolUse` hook now returns the required
  `{"decision":"allow"}` response on normal capture, malformed input, and
  capture-policy drops. Local installs default to the native spool-based hook
  command, whose generic `{}` response caused Antigravity to deny every tool
  call even though the staged shell and PowerShell hooks already returned the
  correct decision (#352).

### Added
- Kiro CLI is now supported as an MCP-only client through `install-mcp
  --client kiro-cli` (alias `kiro`). The installer preserves existing
  `$KIRO_HOME/settings/mcp.json` content, adds bearer headers when configured,
  honors `$KIRO_HOME`, and appends a Bedrock schema flavor that removes only
  unsupported root-level JSON Schema combinators. Non-loopback Kiro endpoints
  must use HTTPS and are rejected before `--apply` writes an unusable config.
  Kiro lifecycle hooks and managed workstreams remain deferred because its v2
  and early-access v3 engines use incompatible hook and session formats
  (#351).
- Added `ai-memory continue`, which resumes the most recently launched managed
  checkout from any directory. `run`'s bare mode already continues the current
  checkout, but its workstream lookup is keyed by repository and worktree
  fingerprints, so it requires a `cd` first. `continue` orders the client-local
  checkout links by their `linked_at` stamp, revalidates the newest one's path
  and resolved scope, and then delegates to the same bare-mode launch. Stale,
  retargeted, scope-mismatched, or corrupt-ordering links are announced on
  stderr and skipped rather than silently resuming a different project. It
  accepts `--workspace`,
  `--yolo`, and `--fresh`; native harness arguments and `--executable` remain
  unavailable because bare mode does not know which harness it will pick.
  Docker-wrapper installs route the command through the checksum-verified host
  client so it can inspect local checkouts, session stores, and harnesses
  (#350).
- New `GET /admin/sessions/by-agent` endpoint reporting how many sessions
  each agent CLI opened in one scope (`claude-code`, `cursor`, `codex`, …),
  so a dashboard can answer "where is this project's memory coming from".
  `sessions.agent_kind` already carried the answer but no read surface
  exposed it — the existing `/admin/open-sessions` takes the agent as a
  *filter* and returns neither the kind nor a total. Counts cover open and
  ended sessions alike, take an optional `since_days` window (`0` or absent
  = whole history), and order count-descending with an agent-name tiebreak
  so equal counts do not reorder between calls. Like the other scoped admin
  reads it reports the caller's own sessions plus unowned ones, with
  `all_owners=true` as the recovery switch. Unknown scopes 404 rather than
  being auto-created. No migration. (#349)

## [1.22.0] - 2026-08-01

### Added
- Added `ai-memory show`, a project-first managed launcher that joins private
  client-local checkout links with public server project metadata and a bounded
  local scan, then launches an installed harness without changing the parent
  process directory. It supports discovery-only `--json`, stages new projects
  atomically with routing and Agent Skills, keeps remote-server filesystem paths
  private, refreshes links after successful managed prepares and CLI
  rename/move operations, and runs host-side through the Docker wrapper (#342).
- Hook ingestion now recognizes Hermes Agent as a concrete session kind and
  understands its documented shell-hook `tool_name` / `tool_input` envelope
  for bounded tool-family titles and capture exclusions. Migration V44 expands
  the session allowlist without losing session ownership, observation
  watermarks, indexes, or scope-pairing triggers. This remains protocol support,
  not a first-party Hermes installer, and session-start handoff acceptance stays
  disabled because Hermes ignores that hook's stdout (#337).
- Trusted-proxy identity now has a dedicated
  `[auth].actor_proxy_bearer_token`, distinct from the root bearer. Proxy
  requests must assert either a username or the complete OIDC issuer/subject
  pair; missing, partial, duplicated, or comma-folded identity headers fail
  closed. Proxied root access requires the configured OIDC issuer/subject pair;
  a display username never grants root. Ordinary root and DB-user requests
  continue to ignore raw actor headers, and leaving the proxy token unset
  preserves existing behavior (#333).
- Qualified identity keys now keep usernames separate from OIDC
  `(issuer, subject)` pairs and drive active-project routing consistently for
  hook and MCP requests. The stable OIDC pair outranks display usernames, so
  same-subject users from different issuers cannot alias each other (#333).
- The `/admin/*` route layer and the MCP `memory_forget_sweep` tool now ask
  "does this deployment distinguish operators" instead of "do `users` rows
  exist", so a trusted-proxy deployment — which never writes a `users` row —
  gets root-only admin gating instead of waving every proxied caller through
  the single-operator escape hatch (#333).
- Optional `[slots] per_user` namespaced engine-written memory slots by the
  authenticated operator. Session briefs and consolidation prompts now receive
  shared slots plus that operator's own bounded namespace, while foreign slot
  writes are refused. The feature defaults off, preserves existing shared
  slots, and intentionally leaves exact wiki reads and searches project-wide
  because it is an agent-context injection boundary rather than RBAC (#335).
- Handoffs now belong to the operator that created them (migration V39). On a
  server shared by several people the open-handoff lookup was scoped by
  `(workspace, project, state)` alone, so the next session to start — whoever it
  belonged to — consumed the pending baton, and delivery is destructive, so the
  author simply lost it. `cwd` did not help: a handoff created through
  `memory_handoff_begin` is always manual (`from_session_id = NULL`), and manual
  handoffs bypass the cwd check and outrank automatic ones, so the deliberate
  artefact was exactly the one that crossed operators. Ownership is now checked
  before those rules, and the owner column holds the qualified
  `IdentityKey::storage_key()` TEXT. `memory_handoff_begin` gains `shared: true`
  to publish a baton to the whole project on purpose; `memory_handoff_accept`
  gains `any_owner: true` for recovery. A NULL owner still means "shared", so
  every stored row and every caller without an authenticated actor behaves
  exactly as before (#334).
- Sessions record their operator (migration V40), and the open-session lookup
  behind `GET /admin/open-sessions` is scoped to the caller unless
  `all_owners=true`. `finalize-session` drives off that lookup and acts
  destructively on the result — ending the session, synthesising a page from its
  observations and minting a handoff carrying its raw prompts — so picking "the
  newest open session in the scope" could do all of that to a colleague's live
  session. The new `--all-owners` flag exposes the server switch (#334).
- New `GET /api/v1/workspaces/{workspace}/projects/{project}/handoffs` lists a
  project's handoffs, filtered by `state` and scoped by owner, backed by new
  non-partial indexes (migration V41) since every pre-existing handoffs index is
  partial on `state = 'open'`. There was no handoff listing anywhere in the
  system: readers only ever fetched the single pending one and consumed it, so a
  mis-delivered baton could not be inspected or recovered. On a server that
  authenticates, the prompt-derived fields (`summary`, `open_questions`,
  `next_steps`) are served to a caller the server can name and to the root
  operator, and are omitted with `redacted: true` for a caller it can place as
  neither — unowned rows are shared, so such a caller matches every one of them,
  and unlike the overview's single newest card this returns the project's whole
  history. Cross-owner reads require the explicit root-only
  `all_owners=true` recovery switch. The metadata is served either way,
  which is what makes the listing useful; a server with no auth configured
  serves the bodies too, since it already serves every page body
  unauthenticated (#334).
- New admission-chain operations `handoff_begin`, `handoff_accept` and
  `handoff_cancel`. Handoffs live in their own table, so their lifecycle never
  passed through `Wiki::write_page` and was invisible to admission webhooks —
  leaving the operations that move prompt-derived text between operators
  unauthorizable. Every path raising one of them asks and announces in the same
  order: only the webhooks that can refuse (blocking + reject policy) are
  awaited before the operation, and observers are dispatched fire-and-forget
  after it, only if it happened — so `memory_handoff_accept`, whose routine
  answer is `{"handoff": null}`, never announces an accept that found nothing,
  and a mirror is never told about a baton the engine then abandoned. The
  automatic SessionEnd handoff and the session-start claim run through the same
  chain as their operator-triggered counterparts, forwarding the caller's
  webhook skip-list under the same root-only rule as every other transport.
  Neither can cost an operator anything beyond the operation the webhook
  declined: SessionEnd still writes the summary page, runs the opt-in
  consolidation and commits (a refusal skips the baton and is logged), and a
  refused, timed-out or unreachable claim leaves the handoff open for the next
  session (#334).
- Pending auto-improve proposals record who staged them (V42
  `staged_by_actor_user`, the qualified identity key, surfaced on the proposal
  detail), and the one-pending-per-target rule is scoped per operator through
  a NULL-collapsing unique index, so one operator's pending suggestion stops
  blocking everybody else's for the same page while every unattributed caller
  keeps the original one-per-page rule unchanged (#336).
- Page reinforcement records each distinct authenticated operator (V43
  `page_access`) beside the existing shared access counter. The opt-in
  `[decay] breadth_weight` term (default `0.0`) lets the forget sweep retain
  pages reinforced by several operators without changing existing scores at
  the default or for pages with zero or one identified reader (#336).

- `run` accepts `antigravity` (aliases `antigravity-cli`, `agy`) as a
  managed harness, so an Antigravity session joins the same workstream as the
  Claude Code or Codex sessions on the same checkout. `agy` accepts no
  caller-chosen id for a new conversation, so a fresh launch injects no
  selector and the id is linked by the hooks or discovered afterwards; a
  linked resume passes `--conversation <id>`, and `--continue` / `-c` is
  respected as an explicit user choice. `--yolo` maps to
  `--dangerously-skip-permissions`, and the utility
  subcommands (`models`, `plugin`, `update`, …) pass through without a selector.
  Conversation discovery reads the per-conversation SQLite databases under
  `~/.gemini/antigravity-cli/conversations/`: the id is the file name and the
  workspace comes from two observed protobuf fields, so only conversations
  opened on the current directory are offered and a database from another `agy`
  version is skipped instead of failing the listing. Step payloads are
  undocumented, unversioned protobuf, so conversation text is deliberately not
  decoded — the visible-event ledger for this harness comes from lifecycle-hook
  capture, and transcript export fails with a message saying so. Antigravity is
  not part of the no-argument auto-detection pool. The native contract was
  verified against Antigravity CLI v1.1.7 (#345).

### Fixed
- `run` on Windows now resolves npm-style harness installs through `PATHEXT`
  and starts the resolved wrapper. This avoids accepting an extensionless Unix
  shell shim such as `opencode` during availability checks and then failing to
  launch it when the adjacent `opencode.cmd` is the Windows entry point. Unix
  resolution remains unchanged (#343).
- `memory_auto_improve` without a `session_id` now selects the newest
  completed session that has no persisted auto-improvement run. Preflight-
  skipped sessions therefore advance the implicit manual-review queue instead
  of permanently starving older sessions; passing an explicit session ID still
  permits a targeted rerun (#338).
- Handoff and session ownership is stamped only where the deployment actually
  distinguishes operators. A server with `[auth].bearer_token` +
  `[auth].root_username`, no `users` rows and no proxy has one operator and two
  transports: stamping that one name on every HTTP write while the stdio /
  in-process transport carries no actor would make one person's handoffs and
  sessions invisible to their own other transport on the same data directory.
  With nobody to separate, the stamp is the pre-ownership `NULL` and both
  transports agree. Reads are deliberately not gated the same way, so rows
  stamped while a deployment did distinguish operators stay readable by that
  operator afterwards (#334).
- The automatic SessionEnd handoff, the session page and both consolidation
  paths attribute to the operator recorded on the **session**, not to whoever
  delivered the event — a spool drain, a shared hook token or an operator
  finalizing a stuck session all carry a different identity. The atomic store
  operation also rejects a handoff whose owner differs from its source session
  (#334).
- Briefings and both read-only overviews — workspace and project — scope
  handoffs to the requesting actor instead of showing only unowned ones, and
  `pending_handoff_count` applies the same filter as the fetch — otherwise a
  briefing advertises a pending baton the same caller can never retrieve, and on
  any server that stamps owners the overview's handoff card would go permanently
  empty while the count beside it kept reporting the row. The read-only
  `/api/v1` overview no longer surfaces handoffs that belong to a specific
  operator, including the raw prompt text an automatic handoff is synthesised
  from, to a browser the server cannot attribute (#334).
- Retiring superseded automatic handoffs no longer crosses an operator
  boundary. Both sweeps — the same-cwd expiry on a new SessionEnd handoff and
  the post-claim cleanup after an accept — match on the acting handoff's
  `owner_user`, so one person starting or ending a session in a directory
  cannot expire another person's pending baton. A shared handoff (no owner) is
  visible to everyone, so it is only ever superseded by another shared one; on
  a single-operator or unauthenticated server every row is unowned and the
  sweeps behave exactly as they did (#334).
- `ops::accept_handoff` propagates whether the claim actually succeeded, so
  `memory_handoff_accept` no longer returns the handoff body when the atomic
  claim was lost — previously two agents could be handed the same baton. The
  cross-operator escape hatches require admin authority: `any_owner` on
  `memory_handoff_accept` and on `memory_handoff_cancel` (which previously had
  no recovery path at all, so a handoff whose owner no longer matched any
  reachable identity could not be discarded), and `--all-owners` on
  `ai-memory finalize-session` (#334).
- The `[auto_scope] per_actor` active-project map is keyed by the qualified
  identity on both sides — the hook ingress that publishes and the MCP tools
  that read — so an OIDC-proxied operator's writes and reads land on the same
  slot instead of silently missing on every read (#334).
- Owner predicates for pending and exact-id handoff reads now execute in SQL
  before prompt-derived fields are loaded. Exact-id cancellation returns the
  same result for absent, wrong-scope and foreign-owner ids, so it no longer
  discloses another operator's handoff state before authorization. Accept and
  cancel also recheck the expected workspace and project in the atomic state
  update, closing a lifecycle race between the scoped read and destructive
  write (#334).
- Ownership writes reject malformed identity keys, and malformed non-null
  session-owner keys already present in the database fail closed during hook
  processing instead of being converted to the shared `NULL` bucket.
  Named-user handoff history uses two indexed shared/owned ranges, so a large
  volume of another operator's rows cannot turn a bounded listing into a
  project-wide scan (#334).
- Actor-scoped briefing, overview and handoff-history responses now use
  `Cache-Control: private, no-store`, preventing a browser from reusing one
  operator's prompt-derived response after credentials at the same URL change
  to another operator (#334).
- Automatic session-start handoff admission is capped at 750 ms, below the
  shortest shipped client's one-second fetch timeout. A slow deciding webhook
  leaves the baton open instead of approving and consuming it after the caller
  has disconnected (#334).
- A staged auto-improve proposal colliding with one already pending no longer
  aborts its whole staging run (losing the run row, its sibling proposals and
  the paid LLM review): the colliding proposal alone is skipped, and every
  staging surface — `memory_auto_improve`, `/admin/auto-improve`, the
  telemetry report, the curator, the CLI and the scheduler's log — names the
  skipped target and the reason instead of silently returning N-1 proposals
  (#336).

## [1.21.0] - 2026-07-31

### Added
- Per-page TTL via a frontmatter `expires_at:` key (RFC3339, or a bare
  `YYYY-MM-DD` meaning end of that day UTC), mirrored into a new
  `pages.expires_at` column (V36) and settable through a new optional
  `expires_at` parameter on `memory_write_page`. Expired pages are hidden
  from `memory_query`/`memory_recent`/briefing/session-brief surfaces —
  `memory_query` gains `include_expired: true` to still see them — while
  exact-path reads still return the page, annotated `expired: true`,
  because an explicit read is not a search. The forget sweep hard-deletes
  them through the wiki layer, so the markdown file goes too, not just
  the rows. An explicit TTL outranks `pinned` (a pin means "don't decay
  this", not "keep it past the date its author set"); `memory_lint`
  flags pinned+expiring pages so the combination is visible rather than
  silent. (#309)
- Zed editor as an MCP-only client. `install-mcp --client zed` renders,
  and `--apply` idempotently merges, a native remote HTTP entry under the
  top-level `context_servers` map in Zed's platform user `settings.json`,
  with optional bearer headers. The JSONC-aware apply and uninstall paths
  preserve user comments, trailing commas, unrelated settings, and sibling
  servers while changing only the matching ai-memory entry. Zed does not
  provide lifecycle hooks or managed-workstream continuity. (#321)
- Entity-match retrieval as a fourth RRF stream (V38 `entities` +
  `entity_page_links`). Consolidation emits up to 10 normalized technologies,
  components, services, files, or domain nouns per page into frontmatter;
  manually edited `entities` use the same index path, and reindex rebuilds the
  derived tables from markdown. Project-scoped query tokens match exact names,
  name prefixes, or word prefixes inside compound names and are weighted by
  inverse entity frequency before RRF fusion and the existing authority and
  optional LLM reranking stages. Empty entity indexes contribute no candidates
  or score, and entity matching makes no LLM call. `explain: true` reports the
  entity stream's rank, raw inverse-frequency weight, contribution, and matched
  names. (#320)
- Optional post-RRF reranking for project and explicit-scope
  `memory_query`, off by default. Set `AI_MEMORY_RERANKER=llm` (requires
  `AI_MEMORY_LLM_PROVIDER`) to over-fetch candidates, fuse scopes, and
  make at most one structured-output call through any existing LLM
  provider. The prompt JSON-encodes untrusted input and sends the query
  (up to 1,000 bytes) plus at most 30 page titles (200 bytes each) and
  snippets (600 bytes each) to that provider. The requested result limit
  is preserved even above 30; only the first 30 candidates are judged.
  A partial/duplicate/unknown id set, invalid score, timeout, provider error,
  or four-call concurrency saturation preserves the pre-rerank order.
  `global=true` and supplemental
  global-preference hits keep their existing non-RRF ranking. With
  `explain: true`, judged hits include `rerank_score`. Unknown reranker
  values and `llm` without a provider fail at startup. (#319)
- New MCP tool `memory_feedback` (17th tool) — the "finer-grained
  reinforcement beyond access counts" P2 item. Record how useful a
  recalled page actually was by exact path: `helpful` / `not_helpful`
  step the page's new `pages.salience` column (V37, bounded to
  `[0.25, 2.0]` in 0.25 steps), which now scales the retention formula's
  time term for sweep-eligible episodic pages instead of a single global
  `salience_default`; `stale` /
  `wrong` floor the salience AND surface the page as a
  `feedback_flagged` finding in the next `memory_lint` report. Signals
  land in a new append-only `page_feedback` table with an optional
  sanitized, bounded single-line reason, the resulting salience needed to
  rebuild derived state, and a full audit-log entry. Nothing is ever
  deleted by feedback. The exact path resolves to the current page version
  in the feedback transaction, so rewriting a flagged page later retires
  both its salience and its lint findings — there is no separate dismissal
  state. Pages without feedback keep `salience = NULL`, which reads as
  exactly the previous behaviour. Retrieved content cannot authorize a
  feedback call; agents treat it as untrusted data. (#318)
- `memory_query` gained an optional `explain: true` mode for project and
  explicit-scope searches. Each compiled-page hit then includes its 1-based
  FTS5, entity, vector, and graph ranks; raw BM25/cosine/entity values; matched
  entity names; graph seed and link direction; per-stream RRF contributions;
  fused score; and bounded authority multiplier. `streams_active` makes vector
  degradation visible. Global
  cross-project search reports its distinct FTS-only stream but does not attach
  RRF details to `global_hits`. Explain provenance is computed only when
  requested. (#317)
- Per-project consolidation instructions: write a reserved
  `_prompts/consolidation.md` wiki page (via `memory_write_page` or on
  disk - no config key) and its body is appended to both single-page and
  multi-page consolidation prompts as advisory preferences ("prefer
  Portuguese titles", "skip CI noise", ...). The block is scrubbed
  through the configured sanitizer, capped at 2,000 characters, JSON-encoded,
  and injected into the LLM user message under an explicitly untrusted,
  schema-subordinate system-prompt contract. `memory_consolidate` also gained
  an optional `instructions` argument that overrides the page for one call;
  TTL-expired standing pages are ignored. (#316)

### Fixed
- Zero-embedding startup and current retrieval descriptions now include the
  entity-match stream, and release notes attribute per-page TTL to the release
  where it shipped. (#329)
- Retrieved `_rules/`, `gotchas/`, `procedures/`, and `decisions/` pages are now
  described consistently across MCP and installed skill prompts as untrusted
  historical evidence, removing contradictory language that elevated stored
  prose into operating policy or constraints. (#325)
- A project-scoped forget sweep now purges aged decay tombstones only from its
  resolved workspace/project instead of deleting eligible derived rows across
  every project. Entity-index rows orphaned by the scoped purge are removed in
  the same transaction, and `hard_deleted` reports only the target scope. (#323)
- Zero-LLM `memory_query` now keeps graph-neighbour expansion active instead
  of falling back to FTS5 alone when no query embedding exists. Equal adjusted
  hybrid and explicit multi-scope scores now use a deterministic path
  tiebreak. (#317)

## [1.20.2] - 2026-07-30

### Fixed
- Docker build contexts now exclude the gitignored operator deployment files,
  preventing local server configuration and production environment secrets
  from being sent to the Docker builder. A packaging regression test keeps the
  exclusions as the final ignore rules so later negations cannot re-include
  them. (#314)
- Managed workstream heartbeats now bound each server request and condense an
  outage into one short notice plus one recovery notice. Active launchers keep
  the lease-safe 30-second retry cadence without printing the same timeout on
  every attempt, and may renew their original run after a longer outage unless
  another launcher has already claimed the workstream. (#311)

## [1.20.1] - 2026-07-30

### Fixed
- Managed workstream packets, handoffs, project briefs, MCP routing prompts,
  and all LLM maintenance prompts now identify stored project material as
  untrusted historical data rather than executable instructions. This limits
  persistent prompt injection through captured prompts, tool output, wiki
  pages, commit messages, or another authenticated user's shared content.
  (#302)
- Docker wrapper installation and self-upgrade now use checksum-verified assets
  from the latest GitHub Release instead of executing the mutable `main` branch.
  The standalone hook installer and hook bundle use the same verified release
  path and install only the expected hook members without extracting arbitrary
  archive paths. Release jobs publish POSIX/Windows wrapper and hook assets with
  SHA-256 companions, all GitHub Actions are commit-pinned, and default
  workflow permissions are read-only outside the release publisher.
  (#302)

## [1.20.0] - 2026-07-30

### Added
- New `openai-compat` embedding provider for self-hosted engines
  (Ollama, LM Studio, vLLM). Set
  `AI_MEMORY_EMBEDDING_PROVIDER=openai-compat` together with explicit
  `AI_MEMORY_EMBEDDING_BASE_URL`, `AI_MEMORY_EMBEDDING_MODEL`, and
  `AI_MEMORY_EMBEDDING_DIM` — there is no safe default model or
  dimensionality for a self-hosted engine, so each is required rather
  than guessed. Unlike the other providers it is keyless: a bearer
  token is sent only when `LLM_API_KEY` is present, for gateways that
  want one. Embeddings are stored under their own
  `provider="openai-compat"` identity, so switching an existing
  `openai`+base-URL setup over changes the stored
  `{provider, model, dim}` triple — run `ai-memory embed --force` to
  re-embed. (#300)

### Fixed
- Antigravity CLI's `PreInvocation` hook now maps only its documented
  `invocationNum = 0` call to ai-memory's synthetic `SessionStart`. Later model
  invocations perform no capture or destructive handoff fetch, so a manual
  handoff created while the conversation winds down remains open for the next
  session. Native Windows hooks also emit Antigravity's required `injectSteps`
  envelope instead of Claude Code's `hookSpecificOutput` shape. (#298)
- Automatic handoff selection now prefers the newest cwd-eligible session over
  a stale, more-specific ancestor. A new automatic handoff expires prior open
  automatic handoffs from the exact cwd, and accepting the winner atomically
  expires older eligible automatic handoffs. Manual and sibling-directory
  handoffs remain open, preventing stale delivery and inflated pending counts.
  (#293)
- OpenAI-compatible providers now send each structured operation's JSON Schema
  through `response_format=json_schema` by default, so local models cannot
  replace consolidation JSON with prose or omit required fields. Explicit
  structured-output capability rejections fall back to the tolerant parser;
  other HTTP failures still propagate, and
  `AI_MEMORY_LLM_COMPAT_STRICT=false` remains the compatibility opt-out. (#292)
- Recognized Antigravity CLI's native file/edit and search tools, applied path
  exclusions to its `TargetFile` operations, and captured bounded successful
  edit content from `toolCall.args` when the hook omits an output field. Generic
  MCP/resource tools remain fail-closed until their path schemas are proven,
  while failed edits retain their error instead of attempted content. (#294)

## [1.19.2] - 2026-07-28

### Fixed
- Source installation no longer fails when `cargo install --path` resolves
  rmcp 1.8, whose `peer_info()` return type differs from rmcp 1.7. CI now checks
  the unlocked source-install resolution separately from the workspace's
  lock-aware gates, while the documented persistent Windows install uses
  `--locked` for reproducibility. (#285)
- Antigravity CLI hook installation and documentation now expose the existing
  agent-aware manual finalizer:
  `ai-memory finalize-session --agent antigravity-cli`. Antigravity's `Stop`
  event ends one execution loop rather than the conversation, so it remains a
  normal observation; the explicit command closes the latest scoped session
  through the canonical SessionEnd path, producing its summary and automatic
  handoff and queueing opt-in consolidation. The docs also clarify that
  `memory_handoff_begin` deliberately creates a session-neutral, project-wide
  manual handoff for every MCP client; attributed handoffs come from canonical
  SessionEnd processing. (#284)

## [1.19.1] - 2026-07-27

### Changed
- Wiki search now applies a bounded source-authority adjustment after FTS5,
  graph, and optional vector candidate generation. Canonical rules, decisions,
  procedures, gotchas, semantic/procedural tiers, `pinned` pages, and
  `canonical` / `active` / `source-of-truth` tags win close relevance contests;
  episodic sessions, `_lint/` output, investigations, and pages tagged
  `superseded`, `historical`, `test-fixture`, or `do-not-answer-from` are
  downgraded but remain searchable. Exact session-only queries still retrieve
  their evidence, and the returned `rank` includes the bounded adjustment so
  multi-scope merging preserves the same order. (#269)
- Client CLI commands now resolve their `(workspace, project)` from the
  nearest `.ai-memory.toml` marker, not just the lifecycle hooks. Previously
  only the hook path read the marker, so a checkout declaring
  `workspace = "acme"` had its captures land in `acme` while `run`,
  `bootstrap`, `search`, `write-page` and every other scope-taking command
  resolved into `default` — the same repository split across two scopes, with
  `ai-memory run`'s managed workstream stranded on the wrong side. Each field
  still prefers an explicit flag; when the marker decides one, the command
  announces the resolved scope on stderr, naming which half the marker
  decided. `AI_MEMORY_IGNORE_MARKER=1` restores the previous resolution for
  one invocation (client commands only — the hooks keep reading the marker).
  `embed --force` without `--project` still fans out across the workspace and
  no longer needs a derivable project name. `ai-memory serve` is unchanged:
  it has no caller cwd, and its `--workspace` / `--project` remain the baked
  fallback for hook events without a usable one. (#259)
- Marker discovery now stays inside its trust boundary when the caller's cwd
  is outside `$HOME`: it walks no higher than the nearest checkout root, or
  checks only cwd for a non-git directory. Workspace-only markers also keep
  the hooks' documented `project = basename(cwd)` behavior for CLI commands,
  including subdirectories and linked worktrees. (#259)

### Fixed
- Scheduled hollow-project cleanup now treats managed workstreams as project
  data. Older projects whose only history is a managed workstream, including
  those with a live run, are no longer cascade-deleted out from under the
  workstream heartbeat or left with orphaned transcript segments. (#279)
- Hybrid search now gives its FTS, vector, and graph streams the same bounded
  candidate window used by authority-aware FTS search. Small result limits no
  longer exclude a canonical page before post-fusion authority ranking can
  promote it, and candidate-limit arithmetic is saturating throughout. (#277)
- Forced workspace deletion now removes the immutable managed-workstream
  segment directories whose database rows are removed by the workspace
  cascade. Its admin report includes workstream/run counts and IDs, and raw
  segment cleanup participates in the existing filesystem partial-failure
  reporting instead of leaving transcript data orphaned. (#275)
- Lossless `move-project` true moves now re-stamp managed workstreams into the
  destination workspace in the same transaction as the project and its other
  denormalized child rows. Previously the project moved while its managed
  workstreams retained the source `workspace_id`, hiding portable history from
  destination-scope lookup and violating the project/workspace pairing
  invariant. The admin response now reports `workstreams_moved`. (#273)
- SessionEnd recovery now commits the ended generation and automatic handoff in
  one SQLite transaction, then lets an already-ended native replay converge the
  remaining wiki commit, durable consolidation enqueue, and ingest-key
  completion. An interruption after `ended_at` can no longer strand a missing
  handoff or permanently pending spool key, and missing or scope/agent-
  mismatched SessionEnd events no longer attempt consolidation recovery against
  an unrelated session. (#271)
- Bare `install-hooks --apply` re-runs, including the Docker wrapper's
  post-upgrade refresh, now preserve an install's baked `repo-root` project
  strategy for every supported hook integration. An explicit
  `--project-strategy basename` still removes the install-wide default. (#267)
- Installer `--apply` modes now write through symlinked agent configuration
  files instead of atomically replacing the symlink itself. Symlink chains and
  dangling final targets are preserved, while backups remain next to the
  user-facing configuration path. (#264)
- SessionEnd re-consolidation now converges by comparing the current
  observation count with a persisted count stamped by the latest completed
  end, instead of comparing independently generated wall-clock timestamps.
  Clock skew could otherwise leave an old observation permanently "new" and
  repeatedly rewrite the same session page, handoff, and opt-in LLM job with no
  agent activity. Existing ended sessions are baselined during migration so an
  upgrade does not enqueue historical catch-up work. (#268)
- Capture exclusions now canonicalize an existing hook working directory
  before matching paths, so filesystem aliases such as macOS `/var` versus
  `/private/var` cannot turn an excluded file event into a spooled event.
  Marker discovery tests likewise accept the canonical path they request.
  (#265)
- Opt-in SessionEnd LLM consolidation now runs from a durable, generation-
  idempotent queue instead of inside the hook batch request. The hook commits
  its deterministic session page and handoff, persists the provider job, and
  returns without waiting for LLM latency; a single bounded worker recovers
  queued or expired-lease work after restart and makes at most five provider
  attempts with backoff. A stale SessionEnd redelivery also repairs the
  enqueue when the original request was cancelled just after `ended_at`, so
  the default hook drain timeout can no longer silently strand the heuristic
  page as the final result. (#265)
- `purge-project` no longer deletes a project out from under a running agent.
  `workstreams` cascades from `projects` and `managed_runs` cascades from
  `workstreams`, so purging a scope that still held a live managed run tore
  out its lease row: the wrapper then failed every heartbeat with
  `409 managed run lease is not active` and the session's transcript never
  reached the ledger. The purge now refuses with a `409` naming the offending
  workstreams unless `--force` is passed, and its report counts the
  `workstreams` and `managed_runs` the cascade removes; their
  `raw/workstreams/<id>/` directories are now removed server-side and included
  in the same filesystem success/failure report instead of being orphaned.
  Those counters previously showed `0 pages, 0 sessions, …` and made such a
  scope look safe to delete. Liveness is the lease, not the row state: a
  crashed wrapper leaves `state = 'active'` behind until the next
  `ai-memory run` sweeps it, so only a lease that has not yet expired blocks
  the purge. `move-project`'s
  copy-purge merge surfaces the same conflict as a `409` naming how many
  pages were already copied, instead of a `500`; its `--force` flag only
  overrides the active-project guard and never destroys a live managed-run
  lease. (#259)

## [1.19.0] - 2026-07-25

### Fixed
- Claude Desktop's rendered Windows MCP instructions now distinguish the
  unpackaged `%APPDATA%` config from the detected MSIX `LocalCache` config, and
  contributor, managed-workstream, and CLI reference docs no longer omit
  recently shipped harnesses or commands. (#256)
- The opt-in managed-workstream real-harness acceptance runner now verifies
  context delivery from the managed-run cursor/acknowledgement state and a new
  persisted assistant event instead of requiring the model to quote a prior
  sentinel. Large Claude Code hook packets can be file-backed, so acceptance no
  longer passes or fails based on whether the model chooses to use `Read`.
  The deterministic fake Grok leg covers the same assertion path. (#242)
- Managed workstream packets now carry a versioned origin marker, and Claude
  Code transcript import excludes a tool result only when its content begins
  with that marker (or the legacy rendered packet header). This prevents a
  large SessionStart packet that Claude persists and later reads from
  `tool-results/` from re-entering the ledger and recursively consuming future
  packet budgets, while ordinary tool results that merely mention the marker
  remain visible. (#241)
- Lifecycle `user-prompt` and `post-compaction` bodies are now truncated
  UTF-8-safely at 16 KiB, while notification and tool excerpts remain capped at
  2 KB. Native hook commands apply the event cap before local spooling or
  transport, the server repeats it for direct and older clients, and the
  sanitized observation boundary independently caps every durable body at
  16 KiB so neither SQLite nor observation FTS can grow to the 10 MiB HTTP
  transport limit. (#249)
- `ai-memory run <harness>` now verifies an ai-memory-injected native resume
  target still exists in that harness's read-only session store. A confirmed
  orphan starts a fresh native session and repoints the same workstream instead
  of retrying the dead id forever. The new wrapper-owned `--fresh` flag forces
  the same per-workstream recovery without a resume attempt or adoption prompt;
  explicit native resume/session/fork selectors remain authoritative and
  cannot be combined with `--fresh`. (#240)
- `install-mcp --client claude-desktop` now detects an MSIX-packaged
  Claude Desktop on Windows and writes to its virtualized
  `AppData\Local\Packages\Claude_<id>\LocalCache\Roaming\Claude\claude_desktop_config.json`
  instead of the plain `%APPDATA%\Claude\` path. Previously this
  silently wrote a config file the running app ignored, so the MCP
  server never appeared after restart. The detector uses Windows'
  resolved local and roaming app-data roots, prefers an existing config
  when multiple package directories exist, and fails with an explicit
  `--config-file` recovery instruction when the active package is
  ambiguous. Unpackaged installs keep resolving to the plain path.
  (#250)

- The Docker wrappers (`bin/ai-memory`, `bin/ai-memory.ps1`) kept stdin
  attached only on a real terminal, so every piped or redirected
  invocation reached the container with a closed stdin. `ai-memory
  write-page --body -` therefore stored a page with frontmatter and an
  empty body while still reporting a successful write, and the same
  applied to any other stdin reader (hooks fed by a pipe). The wrappers
  now always pass `-i`, while `-t` is added only when stdin and stdout are
  both terminals. `AI_MEMORY_NO_TTY=1` disables only TTY allocation and
  no longer disconnects stdin. (#243)
- Managed `SessionStart` delivery now includes a pending single-use
  handoff before the portable workstream ledger and optional project
  brief. Handoff and ledger acknowledgements are claimed together only
  after the complete response has been assembled; a failed or racing
  ledger claim cannot consume the handoff or suppress retry delivery.
  (#235)

### Added
- `install-mcp --client claude-code --session-aware` now registers an
  ai-memory-owned stdio bridge that forwards Claude Code's
  `CLAUDE_CODE_SESSION_ID` as `X-Memory-Actor-Session-Id` on every upstream
  HTTP MCP request. This makes `[auto_scope] mode = "per_session"` effective
  for concurrent Claude Code sessions against local or remote servers while
  leaving the existing static HTTP registration as the default. The bridge
  preserves bearer auth, stateless/stateful HTTP compatibility, and uninstall
  ownership; both Docker wrappers forward the Claude session variable into
  the helper container. (#244)
- `GET /admin/open-sessions` lists open (not yet ended) sessions for one
  workspace/project/agent, newest first (`all=true` returns every match).
  `ai-memory finalize-session` now uses this endpoint instead of opening
  the local SQLite index directly, so every CLI command is a thin HTTP
  client of the running server; the command now requires a reachable
  server and no longer works against an offline data directory. (#236)
- Managed workstream support for Grok Build CLI: `ai-memory run grok` (alias
  `grok-build`) creates fresh sessions with a wrapper-generated `--session-id`,
  resumes linked sessions with `--resume`, maps wrapper `--yolo` onto Grok's
  native `--yolo`/`--always-approve`, and delivers the bounded workstream
  context packet through Grok's native `--rules` flag (system-prompt append,
  acknowledged only after the child spawns). Transcript import reads
  `$GROK_HOME/sessions/*/*/chat_history.jsonl` read-only with a
  prefix-validated cursor and content-hash event ids so rewind-driven journal
  rewrites cannot duplicate history; system prompts and encrypted reasoning
  are excluded as loss annotations, as are the harness-injected `<user_info>`
  and `<system-reminder>` blocks Grok stores inside `user` records (project
  instructions, the skills catalogue, and connected MCP servers), which would
  otherwise leak harness internals into the portable ledger and evict real
  conversation from the startup packet budget. Discovery
  matches checkouts through `summary.json`'s recorded `info.cwd` and honors
  `GROK_HOME`. Grok stays out of the bare-mode automatic pool. Verified
  against Grok Build CLI v0.2.111 ([#237]).

### Changed
- Single-page, batch, and bootstrap consolidation prompts now ask the model
  to connect related wiki pages with path-based wikilinks and to mirror the
  dominant natural language of the source material while preserving code,
  identifiers, paths, commands, error strings, and JSON field names. (#238)
- The M8 access-counter reinforcement now bumps a page's `access_count` and
  `last_accessed_at` at most once per minute instead of on every search that
  returns it. A first sighting still bumps immediately, and a continuously hot
  page remains eligible once per window, while the cooldown map self-prunes to
  the pages searched within the window. This reduces redundant single-writer
  work under bursty or overlapping searches while intentionally making
  `access_count` a coarser retention signal. (#239)

## [1.18.0] - 2026-07-23

### Added
- The store writer only accepts `Sanitized<NewObservation>`: the privacy
  strip is enforced by the type system at the persistence boundary, so an
  unsanitized observation cannot reach disk by construction.
- The session-review sampler now scores a non-empty `Stop` observation just
  below `PreCompact` (88 vs 90) instead of the low `55` prior, so the opt-in
  assistant/Stop excerpt (a late summary or correction) competes for a sampling
  slot while keeping the `UserPrompt` base priority higher. An empty `Stop`
  keeps its low prior. The reviewer still sees only the first 1500 characters
  of any observation body, so a correction inside a long excerpt must fall in
  that window ([#196]).
- Opt-in assistant/Stop capture for Claude Code (#196). When BOTH the server
  (`capture_assistant = true` / `AI_MEMORY_CAPTURE_ASSISTANT=true`) and the
  client (`install-hooks --agent claude-code --capture-assistant`) opt in, a
  Claude Code `Stop` event carries a sanitized, 2 KB-capped excerpt of the
  assistant's final turn as the Stop body. The client sanitizes with the
  built-in patterns and truncates before the excerpt ever touches the spool or
  wire, splicing a versioned `_ai_memory_assistant` marker into the body and a
  `capture_assistant=1` flag onto the event URL; the server re-scrubs with its
  configured `[sanitize]` patterns and re-enforces the 2 KB cap at the
  persistence boundary (never trusting the client's length). Off by default,
  and any gate failure (server off, wrong agent/event, malformed or
  future-versioned marker, empty excerpt) degrades to an empty Stop with the
  same `202` response. Assistant text is privacy-sensitive — see `SECURITY.md`
  for what it can contain and where it flows. Script-fallback installs cannot
  sanitize the field and drop the whole Stop event instead; move to a native
  install to capture it.
- Managed workstreams now support Kimi Code through `ai-memory run kimi`;
  `kimi-code` and `kimi-cli` are accepted aliases for the installed `kimi`
  executable.
  Returning runs resume the linked native session with `--session <id>`;
  fresh sessions are discovered post-exit by exact checkout match through
  `state.json`'s `workDir` (the session bucket name is a one-way hash and is
  never parsed). The adapter reads `$KIMI_CODE_HOME/sessions` (default
  `~/.kimi-code/sessions`) and imports only visible user/assistant/tool
  messages and compaction summaries from `agents/main/wire.jsonl`, excluding
  system prompts, hidden reasoning, hook-injected context (`hook_result` and
  related origins), and subagent transcripts. Wrapper `--yolo` maps to Kimi
  Code's `--yolo`, and kimi joins the automatic bare-run harness pool. The
  deterministic and opt-in real-harness acceptance paths cover Kimi creation,
  transcript import, cross-harness delivery, and returning native resume; the
  native contract was live-verified against Kimi Code v0.29.0.

### Fixed
- Native hook spool retries now reuse a per-entry idempotency key, so a lost
  batch response does not duplicate observations or completed session-end
  effects. The server claims each key atomically with its observation, resumes
  downstream wiki/handoff work when a prior delivery stopped incomplete, and
  skips only events already marked complete. Overlapping deliveries of the same
  key are serialized; keys are project-scoped and expire after the spool retry
  horizon.
- Managed-workstream overview and command-reference documentation now
  consistently includes Kimi Code in the supported and automatic-selection
  harness lists.
- Windows PowerShell fallback hooks now force text output and silence
  non-interactive progress records. This prevents nested PowerShell runners
  such as Antigravity CLI from reporting serialized `CLIXML` progress on every
  hook while preserving the hook's JSON stdout. Existing script-fallback
  installs should rerun `install-hooks --agent <agent> --apply` after upgrading
  ([#224]).
- Kimi Code handoffs are now delivered through the `UserPromptSubmit` hook
  instead of `SessionStart`. Kimi Code fires `SessionStart` but discards the
  hook's stdout/result (verified against kimi-code v0.28.1,
  `packages/agent-core/src/session/index.ts`), so the previous hook consumed
  pending handoffs — legacy and managed — without ever showing them to the
  model. `UserPromptSubmit` stdout is injected as a user message before the
  turn. The native `ai-memory hook` path now also accepts the
  `user-prompt-submit` event token alongside `user-prompt` for kimi handoff
  delivery. Existing native hook commands pick up the correction when the
  binary is upgraded; script-fallback installations must re-run
  `ai-memory install-hooks --agent kimi-code --apply` to refresh their staged
  scripts.
- The `[briefing]` compiled project brief is now gated to once per session
  for Kimi Code: it rides the first user prompt (kimi discards SessionStart
  hook stdout, so session-start delivery is impossible) and later prompts
  keep fetching the handoff without the briefing params, so the server no
  longer recomposes the brief on every prompt. This also works for managed
  handoffs, creates local markers only for opted-in repositories, and retains
  at most 512 markers. Re-briefing after `/clear` is not supported in v1.
- Kimi Code managed transcript cursors now validate the imported byte prefix
  before resuming. When Kimi rewrites `wire.jsonl` in place, ai-memory safely
  replays the journal with stable event IDs instead of seeking into a changed
  record or skipping new events.
- Kimi Code managed transcript extraction now imports native
  `context.append_loop_event` assistant text, tool calls, and tool results.
  Current Kimi journals store model output in those records rather than
  re-appending it as a completed assistant message.
- `ai-memory run kimi server ...` now passes through Kimi Code's deprecated
  but still functional `server` utility command instead of treating it as an
  interactive session launch.

## [1.17.3] - 2026-07-22

### Fixed
- Corrected Docker-wrapper upgrade guidance: `ai-memory upgrade` no longer
  claims a configured remote server is stale when it cannot inspect that
  deployment, and the install guide now makes clear that refreshing Docker
  script hooks does not convert them to native capture-policy commands.

## [1.17.2] - 2026-07-22

### Added
- `ai-memory completions <shell>` prints a shell-completion script for bash,
  zsh, fish, PowerShell, or elvish. The script is generated from the binary's
  own command tree, so it always matches the installed version; install paths
  per shell are documented in `docs/shell-completions.md`. The command reads no
  config and needs no data directory, so it works before `ai-memory init`.

### Changed
- Documented Atlas Cloud through the existing `openai-compat` provider instead
  of adding a redundant provider type, including the endpoint, model, and API
  key mapping needed for deployment.
- The native hook binary now drops any raw assistant-message field (Claude
  Code's `last_assistant_message` on `Stop`) before it can reach the local
  spool or the wire, and drains pre-existing spooled entries with the field
  stripped too. The server applies the same strip defensively on `/hook` and
  `/hook/batch` before building an envelope. This field was already never
  persisted, so there is no behavior change; it closes a raw-text exposure in
  the spool/wire. Optional assistant/Stop capture proposed in #196 remains
  disabled. Upgrading the binary is sufficient for native Claude Code installs;
  script-fallback installs (Docker wrapper, `AI_MEMORY_HOOK_PLATFORM=posix`)
  still send the raw field to the server, which strips it immediately on
  receipt. Closing the local-wire vector requires a native client on the agent
  host and using it to reinstall hooks; running the installer through the
  Docker wrapper only refreshes its scripts ([#196]).

### Fixed
- Removed the unused `syntect` dependency and its `plist`, `quick-xml`,
  `bincode`, and `yaml-rust` transitive dependencies. This eliminates two
  high-severity `quick-xml` denial-of-service advisories and makes the prior
  temporary cargo-audit exceptions unnecessary; ai-memory's rendered Markdown
  behavior is unchanged because the syntax-highlighting crate was never used.
- The Docker wrapper now buffers generated shell completions before streaming
  them to stdout. Piping `ai-memory completions <shell>` into a short-lived
  consumer such as `head` no longer exposes Docker's own broken-pipe error or
  non-zero exit after the native command completed successfully; real helper
  container failures still propagate without printing a partial script.
- The Linux Docker wrapper now detects an SELinux-enforcing host plus a
  SELinux-enabled daemon and adds `--security-opt label=disable` only to
  short-lived helper commands that write bind-mounted host files. This avoids
  `Permission denied` during `install-*`, `setup-agent`, `uninstall`, and
  `backup` without relabeling the user's home directory or changing the
  long-lived server container ([#212]).
- Windows `.ps1` fallback hook commands now use PowerShell `-EncodedCommand`
  instead of embedding `$env:` setup inside a nested quoted command. This
  prevents outer Windows hook runners such as Antigravity CLI from expanding
  the setup before the inner PowerShell process receives it. Existing Windows
  Docker-wrapper installs should rerun `install-hooks --agent <agent> --apply`
  after upgrading ([#214]).
- `install-mcp --client claude-code` and `uninstall` now honour
  `CLAUDE_CONFIG_DIR`: MCP registrations go to
  `$CLAUDE_CONFIG_DIR/.claude.json` when the variable is set (non-empty),
  falling back to `~/.claude.json` otherwise. Uninstall checks both the active
  relocated path and the home default so enabling the variable does not orphan
  a prior default-path registration. The
  dry-run output prints the resolved config path instead of a hardcoded path.
- `install-hooks --agent claude-code` and `setup-agent` follow the same
  resolution for the hooks settings file: `$CLAUDE_CONFIG_DIR/settings.json`
  when set, else `~/.claude/settings.json`. Uninstall checks both locations;
  rendered output and the chmod-600 warning show the resolved path.
- `install-skills --scope global` (claude-code root) installs to
  `$CLAUDE_CONFIG_DIR/skills` when the variable is set, else
  `~/.claude/skills`. `uninstall` sweeps the relocated root alongside the
  home default so installs that predate the env var are still removed.
- The Docker CLI wrapper forwards `CLAUDE_CONFIG_DIR` into its helper
  container for relocated Claude Code configs beneath the existing home bind.

## [1.17.1] - 2026-07-20

### Fixed
- Managed launch failures now cancel their server lease immediately, and a new
  launch waits briefly when the previous launcher is still finalizing. The
  parent launcher also survives terminal interrupts long enough to finish or
  cancel the run, preventing a normal quit-and-reopen from being blocked by the
  90-second crash-recovery lease.
- CLI startup diagnostics now show the configured server URL and use the real
  host name in lease-owner messages, avoiding misleading `localhost` labels for
  remote-server clients.

## [1.17.0] - 2026-07-20

### Added
- `ai-memory run` without a harness now selects among checkout-local Claude
  Code, Codex, OpenCode, Pi, and Crush sessions. Empty workstreams adopt the
  newest session automatically; established workstreams prefer their most
  recently linked available harness. A directory with no matching session
  exits with explicit start commands instead of creating an empty workstream.
- Managed workstreams now support Crush through its project-local read-only
  SQLite transcript and supported `options.global_context_paths` configuration.
  ai-memory never writes the original Crush config or session database; the
  launched Crush process retains normal ownership of its native session writes.
- Wrapper-owned `run --yolo` translates to each harness's native dangerous-mode
  option for Claude Code, Codex, OpenCode, Pi, and Crush.
- A managed-harness contribution protocol documents the native-session,
  read-only import, context-delivery, migration, privacy, and acceptance-test
  requirements for adding agents beyond the currently supported set.

### Changed
- The first interactive `ai-memory run` on an otherwise-empty workstream now
  offers recent native sessions recorded for the same checkout, with the newest
  as the default or an explicit fresh-session choice. Adoption is disabled for
  `--new`, explicit selectors, scripted/noninteractive launches, and any
  workstream already established by another harness, preventing obsolete local
  sessions from contaminating later cross-harness switches.

### Fixed
- Managed utility invocations such as `ai-memory run codex --version` no longer
  discover and import a different process's recently updated native session.
  Passthrough commands also do not fetch or acknowledge managed startup context.
- Managed runs now verify the host harness executable before opening a server
  lease and report how to refresh a stale Docker wrapper. The wrapper path is
  regression-tested to preserve a remote `AI_MEMORY_SERVER_URL`, auth, and the
  host `PATH` without entering Docker, and now retains the `run` subcommand when
  it hands control to the native ai-memory client.
- Corrected stale project-move flags in the marker guide and documented the
  distinction between a server-side project rename and a physical checkout or
  native-session relocation.

## [1.16.0] - 2026-07-20

### Added
- Optional managed cross-harness workstreams via `ai-memory run` for Claude
  Code, Codex, OpenCode, Pi, and OMP. Direct harness launches retain the legacy
  hook/handoff behavior. Managed launches transparently create or resume one
  native session per harness, pass all harness argv through without a `--`
  separator, inject unseen portable context at SessionStart, import visible
  native transcript tails through read-only adapters, and record non-mutating
  repository checkpoints. A lease prevents concurrent writers; deterministic
  event ids, incremental cursors, immutable sanitized JSONL segments, and
  batched idempotent imports make retry and crash recovery explicit. Hidden
  reasoning and private provider records are excluded with loss annotations.
  `ai-memory workstream-search` searches history older than the bounded startup
  packet. The Linux/macOS Docker wrapper uses a checksum-verified cached native
  client for `run` so host agent executables and transcript stores remain
  accessible. A separate opt-in local acceptance runner validates real
  cross-harness delivery and native resume without adding credentialed model
  calls to CI. Generated OpenCode/Pi/OMP integrations reserve managed context
  acknowledgement for their model-visible injection path; OpenCode caches the
  packet per native session so auxiliary model requests cannot consume it
  before the main coding turn. Pi/OMP native `--session-dir` overrides are
  honored by the importer, as are native store environment overrides for all
  five adapters. This includes complete atomic-write temp transcripts left by
  a Pi-family process that exits before its final rename.

### Changed
- Configuring the hosted `anthropic` or `openai` provider without an explicit
  model now selects the documented recommended defaults, `claude-haiku-4-5`
  and `gpt-5.4-mini`, instead of the older `claude-sonnet-4-6` and
  `gpt-4o-mini` fallbacks. Explicit `AI_MEMORY_LLM_MODEL` values are unchanged.
### Fixed
- User-facing help and current-reference documentation now match the shipped
  agent integrations, multi-user activation boundary, MCP URL normalization,
  Pi bridge, and `memory_consolidate` configuration requirements. Local
  documentation links and anchors were also audited and repaired.
- Reader APIs now derive absent page kinds consistently from canonical path
  families. Session, concept, procedure, note, and slot pages no longer surface
  as generic facts, while an explicit frontmatter `kind` still takes precedence
  ([#198]).
- Native lifecycle hooks now accept one leading UTF-8 BOM on JSON stdin, which
  covers PowerShell pipelines on Windows. Other malformed payloads are dropped
  before spool or network delivery with a bounded content-free stderr warning;
  hooks still return `{}` and exit successfully ([#197]).

## [1.15.0] - 2026-07-19

### Added
- Tool lifecycle capture now records safe metadata summaries for the closed
  Claude Code, OpenCode, Pi, and Antigravity schemas: canonical family,
  validated agent-provided call ID where available, and a PostToolUse outcome.
  PreToolUse never stores tool inputs, commands, paths, or arbitrary tool
  names; PostToolUse keeps its bounded response/error excerpt. Stop and
  assistant-message capture remain disabled/deferred ([#190]).
- Per-repository capture exclusions via `[capture] ignore_paths` in the nearest
  `.ai-memory.toml` ([#194]). Supported native hooks and generated
  OpenCode/OMP/Pi/OpenClaw integrations apply the policy before local spool or
  network delivery; `ai-memory hook --check-capture` safely reports a bounded
  local decision. Generated integrations resolve relative file-tool paths from
  the event `cwd`, including nested working directories, rather than from the
  marker directory. This changes no MCP tools and needs no database migration.
- Kimi Code CLI is now a supported MCP + lifecycle-hook integration
  (`--client kimi-code` / `--agent kimi-code`, alias `kimi`). `install-mcp`
  merges an `mcpServers` entry into `~/.kimi-code/mcp.json` with a plain `url`
  (Kimi Code treats `url` with no `transport` field as streamable HTTP) plus
  optional bearer `headers`. `install-hooks` merges `[[hooks]]` entries into
  `~/.kimi-code/config.toml`, preserving the provider/model settings the same
  file holds, and covers 10 events: `SessionStart`, `SessionEnd`,
  `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`
  (Kimi Code reports tool failures separately from successful calls; the
  entry reuses the post-tool-use capture path), `Stop`, `SubagentStart`,
  `SubagentStop`, and `PreCompact`. Both paths honor `$KIMI_CODE_HOME`.
  Handoff injection happens at `SessionStart` through hook stdout, which Kimi
  Code appends to the model context; `setup-agent` and `uninstall` handle the
  new agent like the other first-class integrations. The `install-mcp` entry
  points at `/mcp?flavor=moonshot`: the Moonshot API validates tool parameter
  schemas against a restricted dialect ("moonshot flavored json schema") that
  rejects root-level `anyOf`/`oneOf`/`allOf` combinators — including the
  `anyOf` on `memory_read_page` — and fails the whole session at `tools/list`.
  Requests carrying that flavor receive flat tool schemas from the server;
  every other client keeps receiving the upstream schemas unchanged.
  `uninstall` matches the Kimi Code entry in either URL form, so removing an
  install made before or after this change both work with the default
  `--mcp-url`.
- Grok Build CLI is now a first-class MCP client as well as a hook agent:
  `install-mcp --client grok` renders and `--apply` merges a native HTTP
  entry into `$GROK_HOME/config.toml` (default `~/.grok/config.toml`)
  (`[mcp_servers.ai-memory]` with
  `url` / `enabled` / `[mcp_servers.ai-memory.headers]` — not Codex's
  `http_headers` key). `install-hooks --agent grok` reuses that entry to
  infer server URL and bearer token; `uninstall` strips the MCP table;
  `setup-agent --agent grok` prints the companion `install-mcp` tip.
  Managed routing skills can target `.grok/skills` / `$GROK_HOME/skills`
  (default `~/.grok/skills`) via
  `install-skills --agent grok` and `memory_install_self_routing`
  `target_hints`. Lifecycle capture was already present; handoff injection
  remains unavailable because Grok ignores SessionStart stdout — recover
  via MCP `memory_handoff_accept`.

### Changed
- Scheduled forget sweep and rule-based lint now persist their last successful
  completion across restarts. Overdue or never-run jobs perform one bounded
  startup catch-up, while not-due jobs wait only their remaining interval;
  failures retry without advancing cadence. Embedding backfill remains opt-in
  and does not gain startup catch-up behavior ([#192]).
- `init`-generated token peppers no longer make operational `/admin/*` routes
  root-only before the first user is created. Admin mode now switches
  immediately and fail-closed from a store-backed user-row check; expired users
  still count. Servers refuse startup when user rows exist but either the
  non-empty `[auth].token_pepper` or static `[auth].bearer_token` is missing or
  blank, preventing accidental anonymous admin reopening ([#191]).
- `install-hooks --agent devin` now infers the server URL and bearer token from
  `~/.devin/config.json` when flags and environment settings are absent, keeping
  hooks aligned with an existing Devin MCP registration.
- Grok routing guidance now treats Grok Build CLI as an AGENTS-based client and
  directs its managed skills to `.grok/skills` or `$GROK_HOME/skills` (default
  `~/.grok/skills`). The MCP self-routing payload and installed prompt surface
  now guard those targets;
  Grok (like Zero) still resumes handoffs through `memory_handoff_accept`
  because it ignores SessionStart stdout.
- One-shot client commands (`rename-project`, `status`, `write-page`, …) no
  longer print the "cannot write log files … falling back" warning when the
  log directory is unwritable — they still degrade to temp/stderr logging
  silently. The warning was written for the long-running server (where an
  operator should know persistent logs moved) but fired on every Docker
  thin-client invocation too, where it read as a problem with the command
  itself and its sandbox hint was misleading. `serve` still warns, with
  clearer wording that names both the intended and the fallback location.

## [1.14.0] - 2026-07-15

### Added
- New admin endpoint `POST /admin/merge-workspace {from, to, confirm}`: fold
  every project of one workspace into another, then delete the emptied source
  workspace — completing the workspace CRUD surface alongside rename/delete.
  It is sugar over move-project: each source project runs the same validated
  path, so a project that already exists in the destination merges by content
  (copy-purge under `on_conflict`) while a fresh one is a lossless re-stamp.
  Destructive, so `confirm=true` is required; `force` overrides the
  live-session guard per project. It stops at the first failing project — the
  moves already committed stand, and the failing project plus the source
  workspace are left intact for the operator to resolve and re-run (moves are
  idempotent) ([#187]).

### Fixed
- Hook query values are now percent-encoded with an RFC 3986 allow-list
  (everything outside `A-Za-z0-9-_.~`) in both the native helper and the
  POSIX `hooks/_lib.sh`, and the shell cwd extractor unescapes JSON `\\` /
  `\/`. Previously a Windows cwd like `C:\dev\myproject` went into the query
  string with raw backslashes, which broke the shell-script fallback outside
  Git Bash. The reporter confirmed that native Windows handoff delivery was
  already correct for well-formed JSON; their original failure came from a
  PowerShell pipe adding a UTF-8 BOM. The session-start hook now also prints a
  stderr warning when the handoff fetch fails (server unreachable, bad URL)
  instead of being indistinguishable from "no pending handoff" — exit code
  stays 0, hooks never break the agent. Malformed-payload/BOM diagnostics are
  tracked separately in [#197] ([#188]).
- `install-mcp --server-url` now appends the `/mcp` path when given a base
  URL (the same value `install-hooks --server-url` takes), instead of
  rendering a client config that points at the server root and 404s. The
  suffix join is idempotent, so passing the full `…/mcp` endpoint still
  works unchanged; only a URL deliberately pointing MCP at a path not
  ending in `/mcp` (an exotic reverse-proxy rewrite) would notice
  ([#185]).
- Opening a store whose schema is *newer* than the running binary now fails
  with an actionable error instead of refinery's raw "migration V… is missing
  from the filesystem" wording. When an applied migration is absent from the
  binary's compiled-in set (the data was migrated by a newer ai-memory build),
  the store layer now returns `DataSchemaAhead`, which names the offending
  migration, reports the highest schema version this build ships, and tells the
  operator to run a build at least as new as the one that wrote the data. Every
  other migration failure is unchanged ([#184]).
- `memory_consolidate` now resolves its target `(workspace, project)` from
  where the session's observations actually landed, rather than trusting the
  `sessions` row. A session that adopts its scope marker mid-run keeps a
  session row frozen on the pre-marker scope (`begin_session` uses
  `ON CONFLICT DO NOTHING`), while each observation carries the correct
  per-cwd scope — so a "hybrid" session used to consolidate into the wrong
  project. Resolution now prefers the majority observation scope, then the
  session row, then the server's startup IDs ([#186]).

### Changed
- `memory_consolidate` runs the blocking admission chain up front, before the
  LLM, so a rejected scope/actor fails fast and identically in every mode
  instead of only surfacing at write time — previously a single-page write
  spent the LLM before the 403, and a multi-page / dry-run request ran the
  full completion only for the client to time out before seeing the
  rejection. Consequently `dry_run=true` is now a cheap plan: it runs the
  admission preflight and reports the resolved page path without calling the
  LLM (it no longer returns an LLM-generated body preview); a real
  (non-dry) run still produces the page bodies ([#186]).

## [1.13.0] - 2026-07-14

### Fixed
- `memory_consolidate` now attributes its consolidated page to the request's
  authenticated identity (same derivation as `memory_write_page`) instead of
  a hard-coded anonymous actor — so `last_modified_by` is populated and an
  actor-gated admission webhook authorizes the write by user rather than
  rejecting the empty actor. The automatic session-end / compaction
  consolidation in the hook router is system-initiated and deliberately
  stays anonymous ([#183]).
- `install-mcp` and `install-hooks` now honor an explicit `--server-url` even
  when it matches the compiled default. Previously that value was
  indistinguishable from "flag omitted" and could be overridden by
  `AI_MEMORY_SERVER_URL`, which could write config for the wrong local server
  ([#178]).
- Devin hook capture now derives `cwd` when the native payload omits it:
  payload `cwd` still wins, followed by `DEVIN_PROJECT_DIR`, then the hook
  process working directory. This keeps real Devin `SessionStart` /
  `PostToolUse` fixtures routable without inventing a payload field.
  Payloads without a `session_id` are bridged the same way: a per-host id is
  minted at `SessionStart`, reused for later events, and cleared at
  `SessionEnd`; set `AI_MEMORY_SESSION_ID` in the hook environment to pin an
  externally managed run id. A payload-supplied id always wins ([#178]).

### Changed
- `audit-contamination` no longer flags an observation whose project differs
  from its session's home project (the old `observation_session_drift` CHECK
  B, including the `observations_drifted` summary count and the finding's
  `session_id` field). With per-event cwd resolution, an agent that
  legitimately `cd`s across repos in one session produces exactly that shape
  — it is correct attribution, not contamination — so the check drowned
  multi-repo instances in false positives. CHECK A (`session_wrong_bucket`,
  anchored on the session's own cwd evidence) is unchanged and remains the
  high-precision signal ([#182]).

### Added
- The `[recall] default_global` marker now broadens `memory_recent` too,
  completing the pair started with `memory_query` in v1.12.0: an unscoped
  `memory_recent` from an opted-in repo returns the most-recently-updated
  pages across every project as `global_hits` (workspace + project
  annotated). Explicit `workspace`/`project` arguments still scope exactly
  as before, and the plain project-scoped response shape is unchanged
  ([#181]).
- New read-only admin endpoint `GET /admin/projects`: the authoritative
  `(workspace, project)` inventory with page counts and last-updated
  timestamps. Gives dashboards, exports, and backup/mirror tooling a
  first-class list to reconcile against — a mirror directory whose project
  no longer appears here is an orphan and can be pruned. Root-only in
  multi-user mode, like every `/admin/*` route ([#180]).
- `memory_delete_page` / admin `delete-page` now write one attributed
  `audit_log` row pointing at the deleted page id, in the same transaction
  as the delete — completing the "who deleted the gotcha page about X?"
  trail that purge/rename attribution started. Idempotent no-op deletes
  write nothing. The handoff lifecycle (insert / accept / cancel) is also
  audited, scoped to the handoff's workspace/project with a NULL author by
  design — handoffs are agent/session-keyed, not owned by a DB user
  ([#179]).
- Devin CLI is now a supported MCP + lifecycle-hook integration. `install-mcp
  --client devin` writes Devin's `mcpServers` config, `install-hooks --agent
  devin` writes Devin lifecycle hooks, `setup-agent --agent devin` emits
  host-copyable hook snippets, and `uninstall` removes only ai-memory-owned
  Devin entries. Devin hook capture covers `SessionStart`,
  `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostCompaction`, `Stop`,
  and `SessionEnd`; `PostCompaction` is stored as a dedicated observation kind
  and captures Devin's `summary` field. Devin does not expose subagent hook
  events, so subagent capture is not installed for Devin ([#178]).
- Managed Agent Skills can now target Devin: project installs use
  `.devin/skills`; global installs use `%APPDATA%\devin\skills` on Windows and
  `~/.devin/skills` elsewhere ([#178]).
- The store migration set now admits `devin` as a persisted
  `sessions.agent_kind`, preserving the same workspace/project invariants as
  the other supported agents ([#178]).

## [1.12.0] - 2026-07-12

### Added
- New `.ai-memory.toml` marker section `[briefing] inject_on_session_start
  = "true"` ([#176]): the session-start handoff fetch also returns a
  compiled project brief — pinned / `_rules/` / `_slots/` pages with
  bodies plus recently-updated page titles — injected as agent context so
  a fresh session (or a Claude Code `/clear`, which re-fires SessionStart)
  starts with the architecture instead of re-exploring the codebase.
  `max_chars` caps the brief (default 4000 chars, clamped to 500–20000);
  over-budget pages are truncated or listed by path. The brief is
  recomposed on every opted-in session start (non-consumable, unlike the
  handoff) and appended after any pending handoff. Off by default — it
  costs tokens on every session start.
- Generated OpenCode / OMP / OpenClaw TypeScript integrations now forward
  the `[recall] default_global` marker flag (previously only the native
  hook binary did) and the new `[briefing]` keys, accepting bare
  (unquoted) TOML values for both.
- New `.ai-memory.toml` marker option `[recall] default_global = "true"`
  ([#177]): sessions in the marked tree make an *unscoped* `memory_query`
  behave as `global=true`, so meta-repos that constantly need
  sibling-project context stop passing `global=true` by hand. Strictly
  opt-in and per-repo; explicit `workspace`/`project`/`scopes`/`global`
  arguments always win. While active, unscoped queries return
  cross-project `global_hits` (workspace+project annotated) instead of
  project `hits` + `global_scope_hits`; `memory_recent` stays
  project-scoped (documented follow-up).
- `purge-project` and `rename-project` now write attributed rows to the
  append-only `audit_log`, inside the same transaction as the operation
  itself — so "which user wiped project X?" finally has an answer, and a
  rolled-back rename (name collision) leaves no phantom trail. The
  operator identity comes from the authenticated admin request and is
  `NULL` for single-user/unauthenticated servers; internal sub-purges
  (move-project's copy-purge step, the hook router's self-heal) are
  deliberately not attributed as standalone purges ([#175]).

## [1.11.4] - 2026-07-11

### Fixed
- Scheduled forget sweep, rule-based lint, and opt-in embedding backfill now
  iterate every existing workspace/project scope instead of only the project
  selected at server boot. Embedding backfill remains disabled by default, but
  enabled schedules are now store-wide and can increase provider usage ([#173]).

## [1.11.3] - 2026-07-11

### Changed
- Documented the community-maintained Hermes Agent plugin as a third-party
  bridge rather than first-party ai-memory support, with compatibility and
  secret-handling cautions ([#172]).
- `/admin/delete-workspace` now runs `purge_workspace` admission before
  destructive work, reports filesystem partial failures in `files_failed`, and
  notifies non-blocking mirrors after durable deletion; `/admin/rename-workspace`
  refreshes scope manifests after the SQLite rename.
- OIDC CLI help and token-store test fixtures now use a neutral `ai-memory`
  realm placeholder instead of the old project-specific `serpro` example, and
  the unused LLM retry-exhaustion error variant was removed ([#171]).

### Fixed
- Admin scope mutations now invalidate both legacy and keyed active-project
  state plus hook project caches: project moves retarget live active entries,
  workspace deletes clear affected active/cache entries so deleted scopes are
  not recreated by the next hook, and workspace rename manifest refresh
  failures return success with an explicit warning after the committed SQLite
  rename instead of a misleading 500.
- `/hook/batch` now skips per-source rate-limited items and continues accepting
  later unrelated sources, returns non-contiguous acknowledgements for new spool
  drains, bounds limiter key bytes, keys by actor+session with deterministic
  missing-session fallbacks, and avoids spending source tokens on globally
  saturated events. `AI_MEMORY_HOOK_RATE_PER_SEC` and
  `AI_MEMORY_HOOK_RATE_BURST` are now parsed through typed server config
  instead of the hooks crate reading env directly ([#170]).
- MCP tool calls over the stdio transport now fall back to an anonymous
  synthetic request context when HTTP request parts are absent, while preserving
  real streamable-HTTP auth/session parts when present ([#168]).
- Copy-purge `/admin/move-project` now skips only the webhook named exactly
  `contributors` while copying pages, avoiding redundant per-page contributor
  enrichment on already-enriched frontmatter while preserving other
  `write_page` webhooks and the terminal purge notification ([#167]).
- `memory_read_page` by-path not-found errors now name the resolved
  workspace/project scope, making stale or mis-scoped page paths easier to
  diagnose from MCP clients ([#166]).

## [1.11.2] - 2026-07-11

### Fixed
- `ai-memory install-instructions` and `ai-memory uninstall --only instructions`
  now match only line-anchored routing markers, handle CRLF marker lines, and
  repair one or more exact orphan-tail copies left by older refreshes when the
  snippet body mentioned an end marker inline ([#161]).
- Project creation now emits a warning when the same project name already
  exists in another workspace, helping catch accidental cross-workspace
  misroutes while preserving legal id-namespaced homonyms ([#160]).
- `memory_query` now runs the raw-observation fallback for explicit `scopes`
  requests when compiled wiki pages miss, so scoped cross-project searches can
  still surface bounded raw session matches without falling back to the current
  project ([#159]).
- Upgraded `crossbeam-epoch` to the RUSTSEC-2026-0204 fixed release ([#162]).
- Fixed the Docker wrapper on macOS with rootless Docker so host-config
  commands (`install-mcp`, `install-hooks`, `install-instructions`, and
  related setup/removal commands) keep the `-u 0:0` mapping needed to write
  bind-mounted agent configuration as the invoking host user ([#162]).
- Stabilized the Windows uninstall purge-preview regression test against
  verbatim temp-path spelling ([#162]).

## [1.11.1] - 2026-07-09

### Fixed
- The documented auto-improvement safety invariant "never rewrite pinned
  pages" is now enforced in code, not just in the reviewer prompt: the
  apply path — shared by manual approval and `require_approval = false`
  auto-apply — refuses `update` proposals whose target page is pinned,
  recording the proposal as a conflict with an explicit reason. Unpinning
  the page is the explicit way to allow a rewrite ([#157]).

### Added
- Architectural-decision guidance ([#157]): the managed durable-pages
  Agent Skill now teaches agents to record architectural decisions as
  pinned wiki pages under `decisions/<slug>.md` with ADR structure
  (Status / Context / Decision / Consequences, rejected alternatives
  included) and to supersede with a new page instead of editing history.
  `docs/usage.md` gains an ADR section clarifying that ai-memory never
  touches repository files (a `docs/adr/` tree managed by hand or by an
  external ADR MCP server is outside its write surface) and how the two
  coexist.

## [1.11.0] - 2026-07-09

### Fixed
- Mid-session events can no longer scatter observations into basename
  "fragment" projects (`sources`, `desktop`, …). The hook router now uses
  session-sticky attribution: when an event's session already exists, its
  observations inherit the session's project instead of re-deriving one
  from the event's cwd — closing the hole left between the v0.12.2
  cwd-prefix guard (which keys on `repo_path`) and the #103 rule that
  non-git parents never record one, which together left plain-directory
  projects unprotected against every mid-session `cd subdir/`. Explicit
  `.ai-memory.toml` overrides still win, and cwd derivation still decides
  for session-creating events.
- V27 data-repair migration re-runs the idempotent V19 repair on upgrade,
  re-attributing the fragment observations that accumulated since V19 to
  their sessions' projects and deleting the emptied fragment rows.
  Reserved projects (`scratch`, `_global`) are exempt.

### Added
- The maintenance scheduler now sweeps "hollow" project rows — zero
  pages, sessions, observations, and handoffs — once they are older than
  seven days, shortly after startup and then daily. Nothing exists to
  lose in a hollow row (probe/rename residue), so the sweep needs no
  config; rows holding any data are never touched, and reserved projects
  are exempt.

## [1.10.1] - 2026-07-09

### Fixed
- Every CLI command panicked at startup on read-only filesystems
  (sandboxes like ai-jail) when the log directory already existed but the
  log *file* couldn't be created — the half of the failure the existing
  tempdir fallback didn't cover, because it only triggered on directory-
  creation errors and used the panicking appender constructor. File
  logging now degrades instead of failing: `<data_dir>/logs` → the OS
  temp dir → stderr-only, with a warning naming the exact path that
  failed at each step (so sandbox users know what to `--rw-map`), and
  commands keep working regardless ([#158]).

## [1.10.0] - 2026-07-09

### Added
- Zero (Gitlawb/zero) is now a supported agent/client ([#156]).
  `install-mcp --client zero` merges a native HTTP + bearer entry into
  Zero's `~/.config/zero/config.json` (`mcp.servers` map), and
  `install-hooks --agent zero --apply` merges exec-form lifecycle hooks
  (the native `ai-memory hook` command with an args array — JSON payload
  on stdin, no shell) into `~/.config/zero/hooks.json`, preserving
  third-party hooks in the same file by id prefix. All six Zero events
  are covered: `sessionStart`/`sessionEnd`, `beforeTool`/`afterTool`,
  and `specialistStart`/`specialistStop` (mapped to ai-memory's subagent
  events). Zero discards `sessionStart` hook stdout, so capture and
  handoff creation work but handoff injection does not — recover
  handoffs via the MCP `memory_handoff_accept` tool, same policy as
  Grok. `uninstall` strips the hook entries and the MCP registration;
  `setup-agent --agent zero` prints the config for docker-host flows.
  Support tracks Zero's current formats — it is a fast-moving young
  project and upstream churn will be fixed as reported.

## [1.9.1] - 2026-07-08

### Fixed
- `memory_read_page`'s input schema now encodes the "exactly one of
  `path` or `query`" contract as an `anyOf` whose branches demand the
  key's presence *and* a non-null string, so MCP clients that null-fill
  defaulted arguments (observed with OpenCode 1.17.x) are schema-blocked
  from sending the neither-arg call instead of looping on a server
  error. The runtime error for a bare call is now instructive — it names
  both arguments and shows a concrete `{"path": "notes/topic.md"}`
  example so a looping model can self-correct — and the tool/argument
  descriptions state the MUST-pass-one rule up front ([#155]).

## [1.9.0] - 2026-07-07

### Added
- Global preferences scope ([#154]): standing user/team context that
  should apply to every project — technology choices, code style,
  durable personal conventions — now has a dedicated home. Write with
  `memory_write_page` + `scope: "global"`; the page lands in the
  reserved `_global` project (default workspace, following the
  `_meta.md`/`_pending/` reserved-name convention). Default-scoped
  `memory_query` calls union that scope into every project as a new
  `global_scope_hits` response field — one extra scoped search, not the
  O(projects) `global=true` fan-out — while explicitly scoped queries
  (`workspace`/`project`/`scopes`/`global=true`) are unchanged. The
  scope participates by existence: no config, zero effect until the
  first global write. Event capture never creates or attributes to it —
  a directory or marker override named `_global` falls back to the
  server-default project.

### Fixed
- A session that is resumed under the same id after an early `SessionEnd`
  now re-consolidates when it ends again. The end-of-session guard used to
  drop any `SessionEnd` for a session with `ended_at` set, so a resumed
  session's page stayed frozen at the first end's content and the resumed
  work lived only in raw observations. The guard now distinguishes a
  duplicate/stale end (no observations newer than `ended_at` — still
  dropped, which keeps `finalize-session` and late spool drains safe) from
  a genuine re-end with new work behind it, which re-runs the full end
  path: heuristic page rewrite, `ended_at` bump, auto-handoff refresh, and
  the opt-in LLM consolidation ([#152]).
- `bin/ai-memory` no longer fails `install-mcp`, `install-hooks`,
  `setup-agent`, `install-instructions`, `install-skills`, `uninstall`,
  and `backup` under rootless Docker. Rootless Docker maps container UID
  0 back to the real
  host user but routes any other UID (including the host UID the wrapper
  always passed via `-u`) through an unrelated subordinate-UID range, so
  every write to a bind-mounted host path (`~/.claude/settings.json`,
  the hook staging dir, `$PWD/CLAUDE.md`, skill directories) failed with
  `Permission denied` or a misleading "does not exist" error. The wrapper
  now detects rootless Docker via `docker info --format
  '{{.SecurityOptions}}'` and runs as `-u 0:0` for just the commands that
  write host-side files; thin-client commands (`status`, `bootstrap`, …)
  are unaffected since they only touch the `/data` named volume.

## [1.8.0] - 2026-07-04

### Added
- OpenCode Zen/Go is now a first-class LLM provider:
  `AI_MEMORY_LLM_PROVIDER=opencode` (alias `opencode-zen`) routes
  consolidation through `https://opencode.ai/zen/go/v1` using the OpenAI
  chat-completions wire format. Authenticate with `OPENCODE_API_KEY`
  (key from `opencode.ai/auth`); the default model is `claude-sonnet-4-6`.
  `ai-memory llm-test --provider opencode` exercises it end-to-end ([#147]).
- `docker/docker-compose.yml` now loads provider credentials from a
  gitignored `docker/.env` via `env_file` (optional — existing deployments
  without that file keep working), replacing the commented-out inline
  provider blocks ([#147]).

### Fixed
- Gemini structured output no longer fails with `400 INVALID_ARGUMENT …
  "type" … Proto field is not repeating, cannot start list` when a schema
  contains an optional field. `prepare_schema_for_gemini` now collapses
  schemars' Draft-2020-12 `type` arrays (e.g. `["string", "null"]` for
  `Option<T>`) into Gemini's single `type` + `nullable: true` form, so
  consolidation / auto-improve work again with `gemini-2.5-pro` and other
  Gemini models.
- The detached drainer's `logs/hook-drain.log` now rotates once it exceeds
  1 MiB (previous contents move to `hook-drain.log.old`), so an agent
  pointed at a chronically unreachable server can no longer grow the log
  without bound.
- Windows hook-spool drain locks now treat native lock-violation responses as
  expected lock contention, so overlapping background drains skip cleanly
  instead of failing the single-flight guard.
- Windows wiki checkpoints now fall back to the Git CLI for native libgit2
  path-resolution failures when reopening freshly initialised repos, keeping
  delete and purge operations usable under dot-prefixed temp or wrapper paths.

### Changed
- Post-audit cleanup, no behavior change: the `AI_MEMORY_HOOK_PLATFORM`
  override is parsed in one place instead of three copies, the CLI
  `AgentChoice` → domain `AgentKind` mapping is a single `kind()` method
  instead of three per-command match blocks, the companion importer crate
  is now gated in CI (fmt/clippy/test), and
  `crates/ai-memory-store/src/auto_improve.rs` is fully documented (its
  file-wide `missing_docs` allowance is gone). `AI_MEMORY_HOOKS_HOST_ROOT`
  is now documented in `docs/install.md`.

## [1.7.1] - 2026-07-02

### Fixed
- Acknowledged the new `quick-xml` RustSec advisories in CI for the existing
  `syntect` transitive dependency bucket. `plist` still constrains `quick-xml`
  below the fixed 0.41.x branch, and ai-memory does not parse untrusted XML in
  this path; the ignores keep cargo-audit/cargo-deny focused on actionable
  advisories until the upstream dependency chain can update.

## [1.7.0] - 2026-07-02

### Added
- Added `ai-memory finalize-session`, a supported manual Codex finalization
  flow. It defaults to the latest open Codex session in the current
  workspace/project and posts a synthetic `session-end` hook so summaries,
  handoffs, and auto-improvement eligibility use the canonical SessionEnd path.

### Fixed
- Native hook spool delivery no longer relies only on the cancellation-prone
  `session-end` hook to start the detached drainer. `stop` and `pre-compact`
  also request the background `hook-drain` helper after enqueue, and Unix builds
  use a trusted `setsid` launcher when available before falling back to a
  separate process group.
- OpenCode generated plugins now close sessions from the official
  `session.deleted` event and a deduped best-effort `dispose` fallback, so
  OpenCode sessions can produce automatic session summaries and handoffs without
  duplicate `session-end` emissions.

## [1.6.0] - 2026-07-01

### Fixed
- Documented and regression-tested that `install-instructions` updates only the
  ai-memory marker block, preserves unrelated CLAUDE.md / AGENTS.md content,
  writes backups for existing files, and refuses unmanaged same-name skills
  unless explicitly forced.
- Claude Code WindowsNative hook installs now use Claude's exec form
  (`command` executable plus `args` argv array) for the native `ai-memory.exe`
  hook, avoiding shell/Git Bash/PowerShell command-string mangling. Set
  `AI_MEMORY_HOOK_PLATFORM=windows-bash` before `install-hooks` as a fallback for
  older Claude Code builds; exec form requires a real `.exe`, not `.cmd`/`.bat`
  shims.
- Native `session-end` hooks now enqueue and return quickly, then drain the hook
  spool through a hidden detached `hook-drain` process guarded by a real
  single-flight file lock. Background drains use the new bounded
  `AI_MEMORY_HOOK_BACKGROUND_DRAIN_BUDGET_MINUTES` setting (default 5, max 60),
  while `session-start` cleanup remains synchronous and uses one shared
  `AI_MEMORY_HOOK_START_BUDGET_MINUTES` budget for lock wait plus cleanup drain.
  This supersedes the previous inline `session-end` deferred-drain note and
  `AI_MEMORY_HOOK_END_BUDGET_MINUTES` session-end flush budget.
- Pi is now supported through a generated `~/.pi/agent/extensions/ai-memory.ts`
  TypeScript extension that combines lifecycle capture with an HTTP MCP bridge;
  `install-hooks --agent pi --apply` writes it, while `install-mcp --client pi`
  prints bridge guidance instead of writing an ignored native `mcp.json`.
- Generated OpenCode and OMP TypeScript lifecycle hooks now buffer capture
  posts through a bounded best-effort queue instead of spawning one unbounded
  fetch per event, reducing client-side request bursts while preserving direct
  handoff fetches.
- Corrected the Pi vs Oh My Pi / OMP install split: OMP remains supported via
  `--client omp` / `--agent omp` (or `oh-my-pi`) and writes `.omp` config, while
  real `pi` remains a separate install surface. Users who previously used `pi`
  to mean OMP should switch to `omp` or `oh-my-pi`.

## [1.5.0] - 2026-07-01

### Added
- Per-project `drop_subagent_captures` opt-in. A project sets
  `drop_subagent_captures = "true"` in its `.ai-memory.toml`; the host-side hook
  forwards it (as the `drop_subagent` query flag, alongside the existing
  `workspace`/`project`/`project_strategy` marker fields) so the ingest router
  **accepts but does not persist** that project's subagent-session captures,
  keeping only top-level sessions. A multi-agent harness fans one goal out to
  many subagent sessions, each firing lifecycle hooks; on a small shared
  instance that flood can saturate ingest and bloat the store. Scoping the
  opt-in to the project that asked for it avoids a server-global switch that
  would shed subagent captures for every project on the instance. Captures are
  accepted (HTTP 202 / counted in the `/hook/batch` ack) so clients do not retry
  or spool them, but they are not stored. Detection combines a per-event marker
  (`subagentType` for grok, `agent_type`/`agent_id` for Claude Code) with
  stateful, bounded tracking of subagent session ids: the router seeds the set
  from any marked event and from the newly registered
  `SubagentStart`/`SubagentStop` lifecycle hooks (claude-code and grok), and
  clears it on `SubagentStop`, so the unmarked tail of a subagent session (its
  `user_prompt_submit`/`stop`/`session_end`, which carry no marker) is dropped
  too — not just the marker-bearing tool-use events.

### Fixed
- Native Windows/Git Bash installs now normalize hook cwd, stored project
  `repo_path`, and the home-directory guard consistently across slash styles,
  including legacy rows persisted with backslashes. `AI_MEMORY_HOME` now feeds
  the same home guard as `$HOME`, and Git-backed helpers preserve bare-repo
  fallback semantics while limiting CLI git fallbacks to path/open failures.

## [1.4.1] - 2026-06-28

## [1.4.0] - 2026-06-26

### Changed
- `install-instructions` now refreshes a slim markered CLAUDE.md/AGENTS.md
  snippet and installs or updates managed ai-memory Agent Skills by default,
  with `--no-skills` for snippet-only refreshes and `--skills-*` flags for
  scope, agent family, target root, and forced unmanaged replacement. Added
  `install-skills` for refreshing those prompt-packaging skills directly, and
  `memory_install_self_routing` now returns the slim block, managed skill
  payloads, target hints, and overwrite guidance for agents that install
  routing through MCP.
- Agent-facing routing prompts and auto-scope docs now call out that static MCP
  clients running parallel sessions need explicit scope arguments or a
  session-aware bridge that forwards the real lifecycle-hook session id.

### Added
- The native `session-end` hook now emits a one-line stderr note when the spool
  drain leaves events queued for a later boundary: it reports how many events
  were flushed, how many remain queued, whether any events were dropped as
  undeliverable, and names the knobs to bound the backlog
  (`AI_MEMORY_HOOK_END_BUDGET_MINUTES`, `AI_MEMORY_HOOK_INCREMENTAL_THRESHOLD`),
  turning an otherwise silent, scary cancelled-hook symptom into an actionable,
  self-documenting message. A fully-drained session stays silent. (#130)
- `ai-memory uninstall --only skills` now removes ai-memory-managed Agent Skill
  files from the default project/global `.claude/skills` and `.agents/skills`
  roots after marker validation; custom `--target-dir` skill roots remain a
  manual cleanup path.

### Fixed
- Thin-client HTTP CLI commands (`status`, `search`, `read-page`, `write-page`,
  `delete-page`, `backup`, `embed`, and related admin commands) now fall back to
  a stored OIDC device-flow token from `auth.json` when `AI_MEMORY_AUTH_TOKEN` /
  `[auth].bearer_token` is absent. This sends a bearer for external OIDC-aware
  gateways/bridges; native ai-memory server auth still uses the static root
  bearer or DB-user tokens, and `/admin/*` remains root-only unless a gateway
  translates accepted OIDC auth into upstream auth that ai-memory accepts.
  Static bearer tokens still take precedence.

## [1.3.0] - 2026-06-24

### Added
- `install-hooks --project-strategy repo-root` bakes a default project strategy
  into the generated hooks, so every session resolves its project from the main
  git repo root (collapsing subdirectories and worktrees) without a per-repo
  `.ai-memory.toml` marker — preventing a persistent `cd` into a subdirectory
  from forking memory into a phantom project. A marker's own `project_strategy`
  still takes precedence, and the default (`basename`) bakes nothing, so existing
  installs are unchanged. Covers every delivery path: POSIX/PowerShell hook
  scripts, the native `hook` command, and the OpenCode / OMP / OpenClaw
  TypeScript integrations. (#128)

## [1.2.2] - 2026-06-23

### Fixed
- Long-session consolidation now favors later same-session corrections when the
  observation projection cap forces sampling, and both consolidation prompts now
  instruct the model to treat the most recent/final state as authoritative when
  observations contradict earlier drafts.

## [1.2.1] - 2026-06-23

### Fixed
- Hook spool batch drains now scale the `/hook/batch` request timeout with the
  number of events in the chunk, reducing false timeout retries after a slow
  server has successfully committed the batch.
- Hook spool filenames now include a per-process monotonic suffix so tight loops
  or long-lived helper processes cannot overwrite events created in the same
  millisecond.
- Contamination audits now treat `%` and `_` in stored `repo_path` values as
  literal path bytes, matching runtime cwd-prefix resolution.
- Auto-improve telemetry rejection aggregates now exclude rejected maintenance
  report proposals (`curator_report` and `auto_improve_report`) from learning
  rejection signals.
- Auto-improve eval stdout capping now reads only `MAX+1` bytes before failing
  closed, avoiding a flaky oversized-output test path while preserving the cap.
- Updated `quinn-proto` to clear the new RustSec memory-exhaustion advisory.

## [1.2.0] - 2026-06-23

### Added
- Added the standalone optional `companions/ai-memory-importer` package for
  dry-run-by-default OMC flat Markdown wiki imports through public HTTP APIs;
  it is isolated from the root Cargo workspace and root `cargo test --workspace`.
- Auto-improvement reviews can now stage bounded patch proposals for existing
  `_rules/` and `procedures/` pages using append, add-section, and checked
  replace-section edits, with base-body hashes guarding materialize-to-stage
  races.
- Auto-improvement patch proposals now honor a per-run edit budget, and final
  `_rules/` / `procedures/` pages have configurable token budgets to prevent
  reviewer runs from growing policy or procedure pages too aggressively.
- Auto-improvement now keeps a scoped rejection buffer for human rejects,
  approval conflicts/failures, and validator rejected candidates, then feeds a
  bounded summary into future reviewer prompts to avoid repeated failed edits.
- Auto-improvement can now run an optional operator-supplied executable eval gate
  under `[auto_improve.eval]` after LLM validation and before staging/approval for
  selected targets (default `_rules` and `procedures`). The gate is disabled by
  default, receives proposal JSON on stdin, fails closed on command/timeout/JSON
  errors or insufficient score delta, and records eval failures as rejected
  candidates without running from hook paths.
- Added read-only auto-improvement telemetry reporting via
  `POST /admin/auto-improve/report` and `ai-memory auto-improve-report`, with
  JSON and human CLI output for recent run counts, proposal outcomes, terminal
  rates, and operational findings without staging pending proposals; optional
  `--stage` / `stage: true` creates one pending audit report page for approval.
- Added `docs/auto-improve-eval-gates.md` plus dependency-free Python and shell
  scorer templates for `[auto_improve.eval]` proposal gates.
- Hook spool drains now use `POST /hook/batch` when the server supports it,
  grouping compatible queued lifecycle events into bounded batches to amortize
  remote request latency. Older servers fall back to the existing per-event
  `POST /hook` path.

### Fixed
- Auto-improvement eval gates now apply timeouts to the full child interaction,
  cap stdout at 64 KiB, cap eval rejection evidence, and make direct root admin
  requests inherit server eval defaults unless the request explicitly overrides
  them.
- Hook project routing now stores git repo paths using the incoming cwd's visible
  path spelling, so macOS `/var` vs `/private/var` aliases and symlinked cwd
  aliases still prefix-match later events into the same project.
- Updated `git2` to the patched 0.21 line to clear new RustSec unsoundness
  advisories in libgit2 bindings.
- Native Claude Code hooks now capture array-shaped `tool_response` content and
  recognize the native `user-prompt-submit` event token, restoring prompt text
  and tool output bodies for native installs.
- The headless hook spool now warns (instead of silently swallowing) when it
  cannot persist a bumped retry-attempt count back to disk, matching the v1.1.3
  enqueue-failure fix. A failed atomic rewrite previously lost the in-memory
  attempt bump, so a poison spool entry never reached `MAX_ATTEMPTS` and kept
  retrying on every drain boundary with no operator signal until the 7-day
  age-out pruned it. The warning is sanitized and carries no path (a raw spool
  path can be a Windows verbatim `\\?\…` path).

## [1.1.3] - 2026-06-20

### Fixed
- Native Windows Claude Code hooks no longer silently drop every captured event.
  On Windows, `install-hooks` canonicalizes the data dir, which yields a verbatim
  extended-length path (`\\?\C:\…`), and that prefix was baked verbatim into the
  generated `--data-dir` for each native hook command. At capture time the
  hook-spool write under a `\\?\`-prefixed data dir never lands, and the failure
  was swallowed (`let _ = enqueue(...)`), so every `UserPromptSubmit` /
  `PreToolUse` / `PostToolUse` / `PreCompact` / `Stop` / `SessionEnd` event was
  lost while each hook still exited 0 — only `SessionStart` (handoff retrieval)
  kept working, masking the loss. The data dir is now de-verbatim'd both when
  rendering the hook command (new installs emit a plain `--data-dir`) and when
  the hook resolves its data dir at capture time (an already-installed hook
  recovers on the next session without re-running `install-hooks`), and future
  spool enqueue failures emit a sanitized stderr warning instead of staying fully
  silent. (issue #116)
- `bin/release` now handles changelog entries containing backslashes, so Windows
  path examples cannot abort the release after the version files are updated.

## [1.1.2] - 2026-06-19

### Added
- Documented a migration checklist for users replacing another memory tool:
  export first, scrub secrets, curate legacy material into reviewed Markdown,
  configure one client at a time, and remove stale hooks/plugins/MCP servers only
  after ai-memory capture and retrieval are verified. (issue #115)

### Fixed
- Handoff selection no longer strands a detailed manual handoff behind a vague
  auto one. A manual handoff (`memory_handoff_begin`) typically has no cwd while
  the SessionEnd auto handoff carries the session cwd; the read filtered by exact
  `cwd` equality, so the next session — whose SessionStart hook always sends a
  cwd — silently skipped the cwd-less manual handoff and consumed the cwd-bearing
  auto one instead, leaving the manual one open but unreachable (handoffs have no
  list/search surface). Selection now prefers a manual handoff over an auto one
  deterministically: `memory_handoff_begin` always stores a null
  `from_session_id` and the SessionEnd auto handoff always a non-null one, so
  manual handoffs are treated as project-wide (always candidates, whatever cwd
  they carry) and an explicit baton beats the heuristic one regardless of whether
  the model passed a cwd. Among manual handoffs the most recent wins. Auto
  handoffs are scoped by a cwd path-boundary (a handoff left in `/repo` reaches a
  session in `/repo/api`, never `/repo-other`), with the most specific cwd then
  the most recent as tiebreaks — the cwd-specificity tiebreak applies only to
  auto handoffs, never reordering manual ones. The path-boundary is computed in
  Rust, not SQL `LIKE`, so `%`/`_` in a path cannot act as wildcards.
  The stored cwd is normalized (trailing slash stripped) at insert time so
  trailing-slash drift between agent payloads cannot break the match.
  Cross-project isolation is unchanged: handoffs remain scoped by `workspace_id`
  + `project_id`.

## [1.1.1] - 2026-06-18

### Fixed
- Internal consolidation and auto-improvement prompts now use a deterministic,
  budgeted observation projection that preserves high-signal anchors instead of
  suffix-only observation windows. Manual handoff fields and item lists are
  capped after sanitizer scrub with visible truncation markers so oversized
  handoffs do not overwhelm the next agent context; raw observations remain
  unchanged.
- `project_strategy = "repo-root"` no longer falls back to `basename(cwd)` for
  git worktrees whose directory lives outside the main repo tree when the server
  runs in a container. The server resolves repo-root via libgit2 on the incoming
  `cwd`, which fails when that host path is not visible inside the container, so
  every such worktree became its own project. Lifecycle hooks and generated
  TypeScript plugins now resolve the main repo root host-side — following the
  worktree commondir pointer with `git rev-parse --git-common-dir` (or the same
  Rust/libgit2 helper for native hooks) — and send it as an explicit `project`,
  so linked worktrees collapse to one stable project regardless of where the
  worktree directory lives or how the server is deployed. The hook-side git
  probe is silent outside real work trees, preserving the existing basename
  fallback there. (issue #110)
- Unscoped MCP queries could resolve to the wrong project on a shared
  install. The cwd-to-project resolver recorded the bare working directory
  as a project's `repo_path` whenever the cwd was not inside a git repo
  (which, under the default basename strategy, was always), so opening a
  session in a broad ancestor such as `$HOME` created a row that
  prefix-matched every project nested beneath it and captured their unscoped
  lookups. A project's `repo_path` is now the git working-tree root, or unset
  when the cwd is not inside a git repo, never the bare cwd; under the
  default basename strategy it is recorded only when the cwd is the
  repository root, never a subdirectory. A read-time guard additionally
  refuses to prefix-match a stored `repo_path` equal to the operator's
  `$HOME`, and `ai-memory serve` heals existing installs on startup by
  clearing any `repo_path` that is not a real git working-tree root (such as
  a legacy `~/projects` or `/work` catch-all), not only `$HOME` and the
  filesystem root, while leaving paths it cannot see locally (a remote or
  multi-user client path, or an unmounted drive) untouched. (issue #103)
- macOS Docker quick-start now produces a working native-agent setup out of the
  box (issue #107). Three independent breakages are fixed: (1) the macOS wrapper
  baked the container-only `host.docker.internal` URL into the *host* agent
  config, so MCP and every capture hook failed silently — `install-mcp`,
  `install-hooks`, and `setup-agent` now render the host-reachable
  `http://127.0.0.1:49374`, decoupled from the `host.docker.internal` URL the
  wrapper still uses for its own in-container thin-client commands; (2) those
  thin-client commands were rejected with `403 forbidden host` because the
  server's loopback-only Host allowlist excluded `host.docker.internal` — the
  Docker image now ships it in the default `AI_MEMORY_ALLOWED_HOSTS` (native
  installs stay loopback-only; exposed deployments still override it); and (3)
  `setup-agent`/`install-hooks` could not locate the hooks bundle that ships
  beside the binary in the release tarball (the probe derived a bogus
  `/private/hooks/…`), so the binary-sibling `hooks/` directory is now on the
  discovery search path and `--source` is no longer required.
- Wiki reindex and checkpoint restore now preserve page `tier` and `pinned`
  metadata from frontmatter instead of forcing every reindexed page back to
  semantic/unpinned. Wiki writes now serialize canonical tier/pinned metadata so
  later watcher reconciliation remains idempotent for episodic and pinned pages.
- Bounded heuristic session-page raw observation dumps and single-page
  consolidation prompts so very large sessions cannot re-include unbounded
  `## Raw observations` history (issue #102).
- Hook spool no longer counts a server `429` (saturation / `hook queue full`)
  against a spooled event's `MAX_ATTEMPTS` retry budget: transient backpressure
  keeps the event queued without burning an attempt (`MAX_AGE_MS` still bounds
  it), so a saturation burst no longer silently discards real observations.

### Added
- `GET /admin/audit-contamination` (and `ai-memory audit-contamination`) — a
  read-only, SQL-only structural cross-project contamination audit. Flags
  sessions whose `cwd` longest-prefix-resolves to a different project than the
  one they landed in (the auto-scope-bleed signature, resolved with the same
  prefix logic the runtime uses) and observations whose project disagrees with
  their owning session (a regression tripwire that should stay empty on a
  healthy DB). Optional `?workspace=&project=` scope; reports only, never
  mutates, so it is safe to run on any cadence. Purely semantic mislandings
  (no cwd/session anomaly) are out of scope by design.
- Mid-session hook-spool drain: on `post-tool-use`, once the local spool backlog
  crosses a threshold, the hook runs a tightly time-boxed (~250 ms) catch-up
  drain so a heavy session keeps the backlog flat instead of waiting for the next
  session boundary. Tunable via `AI_MEMORY_HOOK_INCREMENTAL_THRESHOLD`
  (default 32 events).
- Added [`docs/macos.md`](docs/macos.md) covering macOS install paths (prebuilt
  release binary, source build, and the Docker wrapper) and the `posix` vs
  `posix-native` hook platform split, with troubleshooting notes for macOS
  wrapper and hook-discovery issues. Linked it from the README support matrix, the docs
  table, and `docs/install.md`, and bundled it into the macOS release tarballs
  alongside `docs/install.md` (mirroring how the Windows zip ships
  `docs/windows.md`).

## [1.1.0] - 2026-06-16

### Fixed
- Auto-improvement scheduling now scans every known project each tick instead of
  only the server's startup/default project. Scheduler ticks remain
  non-overlapping; if reviewing all projects takes longer than the configured
  interval, the next tick is delayed until the current tick finishes.

## [1.0.11] - 2026-06-15

### Added
- Added a server-side auto-improvement scheduler. When an LLM provider is
  configured, `[auto_improve.scheduler] enabled = true` reviews newly completed
  sessions in the background, stages validated proposals for audit, and then
  follows the normal auto-improvement approval policy.
- Added persistent scheduler state and per-session scheduler claims so upgrades
  from the v1.0.3-era schema do not process historical session backlog
  automatically, and failed scheduled reviews do not retry forever. Manual
  `ai-memory auto-improve --session-id <uuid>` and MCP `memory_auto_improve`
  remain the catch-up path for older or failed scheduled sessions.

### Changed
- Clarified that scheduling and approval are separate: disable automatic review
  with `[auto_improve.scheduler] enabled = false`; keep proposals pending with
  `[auto_improve] require_approval = true`.

## [1.0.10] - 2026-06-15

### Changed
- `ai-memory auto-improve --session-id <uuid>` and `POST /admin/auto-improve`
  now record validated proposals in the pending-writes audit trail and approve
  them immediately through the normal wiki write path. Set
  `[auto_improve] require_approval = true` to keep proposals pending for manual
  review. The MCP `memory_auto_improve` tool uses the same behavior.
- Existing project wikis need no migration. Existing server configs that
  contain the old `[auto_improve] mode = ...` key keep working; the key is now
  ignored and can be removed when convenient.

## [1.0.9] - 2026-06-15

### Fixed
- Clarified architecture and auto-improvement design docs so CLI/admin
  auto-improvement is described with pending-writes storage and approval
  boundaries.

## [1.0.8] - 2026-06-15

### Added
- Added auto-improvement proposal storage: `ai-memory auto-improve` and
  `POST /admin/auto-improve` store validated proposals in durable SQLite-backed
  pending-write rows with non-indexed `_pending/auto-improve/` sidecars. The new
  `ai-memory pending-writes list|show|diff|approve|reject` commands and
  `/admin/pending-writes*` routes let operators review, apply, or reject stored
  proposal bodies through the normal wiki mutation path.
- Added first-release curator support: `ai-memory curator` and
  `POST /admin/curator` run a rule-based, no-LLM, report-only maintenance
  review for an existing workspace/project. Dry-run is the default and writes
  nothing; `--stage` creates exactly one pending report proposal for approval
  through the existing pending-writes queue, without editing pages, deleting
  content, rewriting links, or changing slots.

## [1.0.7] - 2026-06-15

### Added
- Added the initial auto-improvement reviewer: `ai-memory auto-improve
  --session-id <uuid>` calls `POST /admin/auto-improve`, reads one completed
  session, applies preflight noise filters, samples large sessions via the
  consolidated session page plus high-signal observations, asks the configured
  LLM for structured durable wiki edit proposals, and validates
  path/evidence/confidence plus duplicate existing path/title constraints.
  Thresholds remain configurable: confidence, input budget, proposal cap,
  `auto_improve` attribution, and `_pending/auto-improve` proposal path.
- Added MCP tool `memory_auto_improve` so agents can run learning review for the
  latest completed session (or a named session) without shelling out. The
  canonical MCP instructions and installed CLAUDE.md/AGENTS.md routing snippet
  now teach agents to treat `_rules/`, `gotchas/`, `procedures/`, and
  `decisions/` as actionable guidance for proactive retrieval.

### Changed
- Clarified upgrade guidance: Docker-wrapper users should run
  `ai-memory upgrade` on each agent machine to refresh the wrapper, pulled
  image, and staged hook scripts; native package/source installs should rerun
  `install-hooks --apply` after binary upgrades; remote servers still need a
  separate redeploy; and existing projects can refresh the managed routing block
  to pick up new proactive retrieval and `memory_auto_improve` guidance.

### Fixed
- `auto-improve` now tolerates common malformed LLM proposal shapes found during
  live testing: evidence arrays may contain bare quote strings instead of
  `{ page, quote }` objects, a missing `operation` defaults to the only supported
  `create_or_update` operation, markdown bodies can arrive as `body`,
  `markdown`, or `content`, plural/folder-style kind names are normalized before
  path validation, and proposals still missing required target data are rejected
  by validation instead of turning the whole review into a 502 response.
- Admin destructive ops (`/admin/purge-project`, `/admin/move-project`) now
  propagate the authenticated actor (from the auth middleware's
  `Extension<ActorContext>`) into the admission context, so a `scope-guard`
  admission webhook can authorize them by user. They previously built the
  context with an empty actor, which a per-user scope-guard ACL rejected with
  `403 user '' not allowed to purge_project`, making purge/move unusable on any
  instance running scope-guard. `rename-project` is unaffected (it runs no
  admission chain).
## [1.0.6] - 2026-06-14

### Changed
- Centralized workspace/project scope resolution in `ai_memory_store::ScopeResolver`
  and shared explicit helpers, then migrated MCP, admin, and web API routes onto
  the common no-create/create-on-write policies.
- Centralized auth checks behind `AuthLevel::authorize(Capability::...)` so admin,
  user-management, and admission-skip behavior share one permission framework.
- Tightened wiki/SQLite consistency semantics: markdown remains the source of
  truth, batch writes install files before committing the derived SQL index, and
  runtime SQL failures roll installed files back best-effort.

### Added
- Regression tests for shared scope policies, capability authorization, and
  wiki-file rollback when `write_page` / `apply_batch` store upserts fail.

## [1.0.5] - 2026-06-14

### Fixed
- Multi-user admin authorization is now enforced at the shared `/admin/*`
  router boundary, including user-management routes, so DB-user tokens can use
  normal MCP/API read-write surfaces for attribution but cannot reach any admin
  endpoint. Added regression coverage for every admin route plus DB-user write
  attribution.

## [1.0.4] - 2026-06-14
### Added
- `install-hooks --agent grok` (plus `setup-agent` / `uninstall` coverage) for
  the xAI **Grok Build CLI**. Grok's `~/.grok/hooks/ai-memory.json` shares Claude
  Code's JSON shape and seven-event vocabulary, with a Grok-specific hook bundle
  and native `ai-memory hook --event … --agent grok` commands.
  ai-memory entries merge into a dedicated `ai-memory.json`, leaving any
  third-party `~/.grok/hooks/*.json` untouched. NOTE: Grok ignores hook stdout on
  `SessionStart`, so capture works but handoff injection does not. Grok's
  `session-start` therefore skips the handoff fetch entirely — the fetch is
  destructive (it marks the handoff accepted server-side) and Grok would discard
  the result, silently losing the handoff; recover a prior session's handoff via
  the MCP `memory_handoff_accept` tool instead. Adds `AgentKind::Grok` (`grok`
  wire tag), the `AgentKind::session_start_injects_handoff` gate, and migration
  `V20` extending the `sessions.agent_kind` CHECK so Grok sessions persist on
  upgraded servers (the antigravity/`V11` precedent).

### Fixed
- Actor-scoped MCP tool calls no longer fall through to a user's or process's
  latest hook-published project when the request carries a session id that does
  not match hook activity. This prevents HTTP-remote shared servers from reading
  or writing another same-user project when the MCP transport session id differs
  from the hook session id ([#97]).
- `memory_handoff_begin` and `memory_handoff_accept` now accept an optional
  `workspace` argument alongside `project`, resolving through the same
  workspace+project path as `memory_write_page` (begin, create-if-missing) and
  `memory_handoff_cancel` (accept, find-only). They previously took `project`
  only, so a cross-workspace handoff was routed by the per-actor active-project
  fallback and could be written to — or read from — the wrong project.
  `memory_handoff_cancel` already carried `workspace`.
- Workspace/project resolution now fails closed across the MCP and admin
  surfaces. Explicit MCP project misses no longer fall back to the active/default
  project, write-style MCP calls reject `workspace` without `project`, admin
  read/search/embed/lint/sweep paths use no-create lookups, and `/admin/reorg`
  only moves sessions and graveyards latest pages inside the target workspace.

## [1.0.3] - 2026-06-13
### Added
- Native macOS release tarballs (`ai-memory-macos-aarch64.tar.gz` for
  Apple Silicon and `ai-memory-macos-x86_64.tar.gz` for Intel) are now
  published on every tag, alongside the existing Linux tarballs and the
  Windows zip. The macOS `release-build` CI job also runs on every push,
  so a macOS-only release regression is caught before the tag rather
  than after. Install instructions added to `README.md` and
  `docs/install.md` ([#94]).

## [1.0.2] - 2026-06-12
### Fixed
- Session-page tool-call counts no longer double every entry. The
  no-LLM synthesizer was counting both `PreToolUse` and `PostToolUse`
  observations into the same bucket, so a single Bash call rendered
  as `Bash: 2` and two real calls as `Bash: 4`. It now counts only
  `PostToolUse` (the "completed call" event), matching the user-facing
  meaning of the heading.

## [1.0.1] - 2026-06-12
### Added
- `install-mcp --client vscode-copilot` renders (and `--apply` writes) a
  workspace-scoped `.vscode/mcp.json` for VS Code GitHub Copilot's agent
  mode. The renderer uses VS Code's MCP framework schema — top-level
  `servers` key, `type: "http"`, `url`, and an inline `headers` map for
  the bearer token — and includes a note that VS Code Copilot does not
  yet expose lifecycle hooks, so ai-memory's automatic capture is not
  active there (the MCP tools must be called explicitly from chat).
  Aliases: `copilot`, `github-copilot`. `uninstall --only mcp` strips
  the same entry idempotently.

## [1.0.0] - 2026-06-12
### Added
- Native hook drain and handoff timings can now be raised with
  `AI_MEMORY_HOOK_DRAIN_TIMEOUT_MINUTES`,
  `AI_MEMORY_HOOK_HANDOFF_TIMEOUT_MINUTES`,
  `AI_MEMORY_HOOK_START_BUDGET_MINUTES`, and
  `AI_MEMORY_HOOK_END_BUDGET_MINUTES` for high-latency or large-backlog
  instances. Defaults preserve the existing short hook behavior; invalid,
  zero, or overly large values fall back or clamp safely.

## [0.16.0] - 2026-06-11
### Added
- Native Claude Code hooks on macOS/Linux now use the direct
  `ai-memory hook --event ...` command by default, matching native Windows.
  Native hook commands spool events locally and can authenticate with a stored
  per-developer OIDC device token instead of a shared static hook token.
- `ai-memory auth login oidc-device --issuer <url> --client-id <id>` stores a
  generic OIDC device-flow token for native hook authentication.

### Fixed
- `ai-memory uninstall --only hooks` now recognizes and removes native
  `ai-memory hook ...` commands as well as legacy script commands.

## [0.15.0] - 2026-06-11
### Added
- Wiki recovery checkpoints now have first-class operator commands:
  `ai-memory checkpoints` lists recent wiki git commits and
  `ai-memory restore-page --path <page.md> --from <rev>` restores one page
  from a checkpoint, writes a new post-restore checkpoint, and reindexes the
  restored page into SQLite. Startup also creates a one-time upgrade baseline
  checkpoint for existing wiki trees that had no git commits yet.

### Fixed
- Release publication now limits the GitHub Release asset download step to
  `ai-memory-*` artifacts, avoiding Docker Buildx side artifacts that can make
  tag workflows fail after binaries and Docker images are already published.

## [0.14.0] - 2026-06-11
### Added
- Tagged releases now publish a native Windows x86_64 zip artifact
  (`ai-memory-windows-x86_64.zip`) with `ai-memory.exe`, hooks, default
  config template, checksums, and Windows install docs, giving native
  Windows agents a no-toolchain path to the fast direct-binary hook mode.

## [0.13.0] - 2026-06-08
### Added
- New `ai-memory reindex` lifecycle command rebuilds the derived SQLite page
  index from the on-disk wiki. It recreates workspace/project rows from
  per-scope `_meta.md` manifests while preserving the UUIDs encoded in the
  wiki tree, then reindexes page markdown into pages, links, and FTS. The
  command refuses to run unless the SQLite store is clean; operators should
  stop the server, back up data, move/remove `db/memory.sqlite`, run
  `reindex`, and recompute embeddings separately with `embed` when needed.
- Wiki startup now backfills per-workspace and per-project `_meta.md` manifests
  containing the human workspace/project names (and `repo_path` for projects)
  so the markdown tree is self-describing enough to rebuild the derived DB.

### Fixed
- Wiki reindexing now treats `log.md` / `log-YYYY-MM.md` as raw hook ledgers
  only when their content opens with the hook log prefix, so ordinary markdown
  pages with reserved-looking names are no longer silently dropped. `_meta.md`
  manifests and direct watcher events also reject symlinks before reading.

## [0.12.3] - 2026-06-07

## [0.12.2] - 2026-06-07
### Fixed
- The hook router no longer auto-creates fragment "projects" for
  subdirectory cwds. When a tool call's cwd sits inside an existing
  project's `repo_path` tree (for example a `Read` of
  `manga-plus/reader/src/main.rs` while the session is attributed to
  `manga-plus`), the resolver now picks the existing parent project
  instead of materialising a `src` or `reader` project for the
  subdirectory name. The schema column `projects.repo_path` has
  always been there for exactly this kind of cwd matching; the
  resolver finally queries it. Sub-projects declared via
  `.ai-memory.toml` still win because their longer `repo_path` ranks
  ahead. New `find_project_by_cwd_prefix` reader helper covers the
  query. Three regression tests in `ai-memory-hooks::router::tests`
  pin the parent / sub-project / cold-start paths.
- V19 data-repair migration re-attributes pre-existing orphan
  observations and handoffs to their session's project, then deletes
  the now-truly-empty fragment project rows that were left behind
  by the bug above. The session is the source of truth — observations
  belong to the session that emitted them and the FK enforces
  session existence. The migration is idempotent (re-running on a
  repaired DB is a no-op) and runs once per data directory at server
  startup via the existing refinery chain. `scratch` is explicitly
  preserved per CLAUDE.md invariant #15a (defensive cwd-less
  default).

## [0.12.1] - 2026-06-07
### Fixed
- `GET /favicon.ico` now lives at the absolute host root, outside
  `--base-path` and outside the `/web` nest, so the browser's
  automatic favicon fetch actually reaches it. The 0.12.0 build mounted
  the route inside the web router and ended up serving the icon only
  at `/web/favicon.ico` — invisible to the browser's auto-fetch (the
  icon still appeared via the in-page `<link rel="icon">` tag, so the
  user-visible behaviour stayed correct, but the dedicated route was
  unreachable). The new mount is also exempt from bearer auth and the
  host allowlist so a fresh tab gets the icon without an HTTP Basic
  prompt; the embedded PNG is the same one any visitor to `/web`
  already sees, so the info-leak surface is nil. Surfaced by the
  post-merge audit live test ([#79]).

## [0.12.0] - 2026-06-07
### Added
- New `ai-memory hook` subcommand emits one lifecycle event natively (reads
  the JSON payload from stdin, POSTs to `/hook`, GETs `/handoff` on
  `session-start`) without spawning a shell. On native Windows, Claude Code
  now defaults to the native hook command — measured ~3.5-5× faster per
  tool-call hook (~735 ms shell → ~150-205 ms native on an i7-6700HQ).
  Opt back into the previous Git Bash + `.sh` path with
  `AI_MEMORY_HOOK_PLATFORM=windows-bash`. See
  [`docs/windows.md`](docs/windows.md#native-hook-command-claude-code-on-windows)
  ([#84]).
- New `GET /favicon.ico` route on the web UI serves the same logo bytes as
  the header, so the browser tab carries an icon without an extra asset
  embed ([#79]).
- Thin-client CLI commands (`status`, `write-page`, `search`, `read-page`,
  `embed`, `lint`, `backup`, …) now respect the server's base-path mount
  via `AI_MEMORY_BASE_PATH` or the path component of `AI_MEMORY_SERVER_URL`
  (URL path wins), so deployments hosted behind a reverse proxy under a
  subpath stop 404'ing — including the container `HEALTHCHECK`
  (`ai-memory status`). Empty / unset means root mount, byte-identical to
  the prior behaviour ([#82]).

### Changed
- The embedded web UI ships a single transparent 768×768 PNG (~126 KB,
  down from a 992 KB JPEG mislabelled as PNG) used for both the header
  logo and the favicon. README branding stays on the existing
  light/dark pair via `<picture>` ([#79]).

### Fixed
- `install-hooks --apply` (and `ai-memory upgrade`, which calls it) now
  MERGES into per-event hook arrays instead of replacing them, so
  third-party hooks registered under the same event (e.g. a context-mode
  `SessionStart` guard) survive re-apply. ai-memory-owned entries are
  still swapped for the fresh ones; re-runs stay idempotent. Resolves
  #80 ([#83]).
- FTS5 searches for filenames carrying ASCII punctuation no longer error
  or silently miss. `current.md` (which used to surface
  `fts5: syntax error near "."`) and `ui-refresh` (which silently returned
  zero hits despite `follow-ups/ui-refresh-scroll-restoration.md` existing)
  both work end-to-end. Punctuated tokens are now quoted as both
  whole-form and split-form phrases, OR'd, to satisfy the asymmetry
  between the content tokenizer (`tokenchars '/_-'` keeps them inside
  tokens) and the path index (which pre-expands `/_-.` to spaces)
  ([#81]).

## [0.11.0] - 2026-06-05
### Added
- New `[auto_scope]` config block (`mode`, `session_ttl_secs`,
  `max_entries`) selects how the hook-published "currently active project"
  pointer is shared across concurrent MCP callers. The default `single` mode
  preserves the historical process-wide slot. Opt-in `per_session` keys the
  pointer by `session_id` to isolate concurrent agent runs of the same
  operator; opt-in `per_actor` keys by `(user, session_id)` to isolate
  across operators as well, pairing with multi-user mode where `user`
  comes from the `users` row that owns the bearer token. `per_actor`
  also keeps a user-only fallback slot so authenticated MCP requests
  from clients that cannot forward a session id do not inherit another
  user's latest project; same-user session isolation still requires a
  client/bridge that sends `X-Memory-Actor-Session-Id` or
  `Mcp-Session-Id` on MCP tool calls. Per-key entries carry an insertion
  timestamp and are TTL-evicted (default 1 hour) and
  capped (default 4096) so adversarial / runaway clients cannot grow the
  map without bound. Both opt-in modes still publish to the single slot
  in parallel, so any caller without actor context falls back gracefully
  to the most recent project rather than an empty pointer. All MCP read
  tools (`memory_query`, `memory_recent`, `memory_read_page`,
  `memory_status`, `memory_briefing`, `memory_explore`, `memory_lint`,
  `memory_forget_sweep`, `memory_handoff_*`) now thread the request's
  `ActorContext` into scope resolution, so opt-in isolation takes effect
  for the full read surface.

### Fixed
- Claude Code lifecycle hooks now emit structured JSON on stdout. Fire-and-
  forget hooks return `{}`, and `SessionStart` wraps pending handoff text in
  `hookSpecificOutput.additionalContext`, avoiding Claude Code's repeated
  "Hook output does not start with {" debug spam while preserving handoff
  injection.
- `POST /admin/rename-project` now returns `404 Not Found` when the project row
  has been deleted (typically by a concurrent `purge-project`) between the
  handler's id lookup and the writer's `UPDATE`. The pre-fix path silently
  responded `200 OK` with `pages: 0` for an operation that affected zero rows,
  which contradicted the concurrent purge's also-`200 OK` destruction of the
  same project and gave operators no signal that the rename had been undone.

## [0.10.0] - 2026-06-04
### Added
- New `POST /admin/delete-page` HTTP endpoint deletes a single page with
  explicit `(workspace, project)`. Like `purge-project`/`rename-project`, it
  uses no-create lookup — a delete on a typo'd or wrong scope now returns
  `404 workspace 'X' not found` instead of silently auto-creating the
  container and returning misleading `deleted: true`.
- New `ai-memory delete-page --path <P> --workspace <W> --project <P>` CLI
  subcommand, a thin client of `/admin/delete-page`. Mirrors the
  write-page/read-page CLI shape so terminal users get a complete
  delete-single-page surface for the first time.
- New `memory_handoff_cancel` MCP tool marks an exact open handoff id expired,
  giving agents a safe way to discard a mistakenly-created pending handoff
  before the next session consumes stale context.

### Fixed
- MCP tool descriptions and routing snippets now draw a sharper boundary
  between read-only `memory_briefing` and session-ending
  `memory_handoff_begin`, reducing accidental dangling handoffs when an agent
  was only asked for project status.
- Custom `--web-ui-dir` SPAs mounted at a non-root `--web-slug` now serve the
  injected shell at the trailing-slash root too (for example `/web/`), matching
  `/web` and deep client routes instead of returning a refresh-only 404.
- OpenCode and OMP generated hooks now derive `project_strategy = "repo-root"`
  project names from the host-visible `.ai-memory.toml` marker directory before
  sending hook payloads, so dockerized servers no longer fall back to git
  discovery inside paths they cannot see.
- `memory_delete_page` (MCP) now accepts `workspace` alongside `project` and
  routes scope through `effective_ids_for_read_args`, the same path the read
  tools use. Previously a project name that lived in multiple workspaces
  could silently route the delete to the wrong slot and return `deleted:
  true` for a page that was never touched. Operators on shared (multi-
  workspace) servers should explicitly pass `workspace + project` to make
  the target unambiguous.

## [0.9.0] - 2026-06-02
### Added
- `openai-compat` LLM providers can now opt into strict JSON Schema structured
  output with `AI_MEMORY_LLM_COMPAT_STRICT=true`. Strict mode sends
  `response_format=json_schema` first for compatible Ollama, vLLM, LM Studio,
  llama.cpp, and gateway endpoints, while the tolerant JSON-object parser
  remains the default and the fallback for strict raw-call failures ([#70]).
- The read-only web browser now renders `[[wiki links]]` as clickable internal
  links to the target page. Supports `[[path]]`, `[[path|label]]`,
  `[[project:path]]`, and `[[workspace/project:path]]`, resolved against the
  current page's project unless the target carries its own scope; bare targets
  get a `.md` suffix. External schemes, path traversal, and links inside fenced
  or inline code are left as literal text ([#68]).
- `ai-memory serve --transport http` can host the entire HTTP surface under a
  configurable subpath with `--base-path` / `AI_MEMORY_BASE_PATH`; `/mcp`,
  `/hook`, `/admin/*`, `/api/v1`, and the web UI all move under that prefix.
  The web UI mount can also be changed with `--web-slug`, and custom
  `--web-ui-dir` SPAs receive injected `<base href>` plus
  `ai-memory-base-path` metadata for same-origin API calls behind reverse
  proxies ([#65]).
- `ai-memory move-project` can move projects across workspaces via the admin
  API. Fresh destinations use a lossless true move that keeps the same
  `project_id`, sessions, observations, handoffs, embeddings, and page history;
  existing same-named destination projects use copy-purge merge with explicit
  `on_conflict` handling. Admission webhooks can subscribe to the new
  `move_project` event and receive destination names in the context ([#60]).
- Page FTS now indexes normalized page paths, so searches can find pages by
  filename or slug even when the slug does not appear in the title/body ([#62]).
- Admission webhooks can now observe, mutate, or reject engine write/delete/
  purge operations, with authenticated actor context, loop-prevention skip
  lists for trusted re-entry, and non-blocking observer webhooks for mirrors
  and backups ([#55]).
- New `memory_delete_page` MCP tool deletes a single page by exact path,
  updates the SQLite index directly, and fires `op=delete` admission hooks
  before removal ([#55]).

### Fixed
- Backups no longer dereference symlinks under `wiki/`, preventing a planted
  wiki symlink from pulling arbitrary readable host-file contents into
  `backup.tar.gz`.
- `ai-memory restore` now validates tar entries before extraction and accepts
  only regular files/directories under the expected backup paths
  (`wiki/`, `db/memory.sqlite`, and `config.toml`), rejecting links, special
  files, unsafe paths, and unexpected archive entries.
- In multi-user mode (`[auth].token_pepper` configured), operational
  `/admin/*` endpoints now require the root token; DB-user tokens receive
  403 while single-user installs keep the historical permissive admin behavior.
- LLM provider clients now cap provider response bodies before JSON, text, or
  SSE parsing, and truncate error bodies from bounded buffers instead of
  buffering arbitrary-size responses.
- Non-blocking admission webhooks now have a process-level in-flight cap and
  webhook timeouts are clamped to a safe maximum, preventing observer hooks
  from growing unbounded background work during write bursts.
- Hook cwd/project resolution caching is now bounded with LRU-style eviction,
  preventing unbounded process-lifetime growth from streams of unique cwd
  values.
- `memory_write_page` tool description and routing prompts now steer agents
  toward writing the page title as a `# H1` on the first line of `body` and
  omitting the `title` argument. ai-memory already auto-derived the title from
  `# H1` (or path stem) when `title` was missing — the change is documentation
  only, but it eliminates a known source of MCP `JSON parsing` errors when the
  LLM failed to escape quotes/colons in `title` ([#67]).
- Custom `--web-ui-dir` frontends no longer serve raw `/index.html` without
  base-path injection; direct index requests and SPA fallback routes now return
  the injected shell, while static assets remain untouched ([#65]).
- `move-project` true moves now run through a wiki mutation gate: normal
  page writes/reindexes validate the `(workspace_id, project_id)` pair before
  touching disk, while true moves hold the exclusive side across the directory
  rename and DB re-stamp. Stale old-workspace writes now fail without creating
  orphan files, and V18 aborts if existing split-brain rows are present ([#60]).
- `move-project` copy-purge conflict detection now treats body, frontmatter,
  title, tier, and pinned status as the page identity under `on_conflict=block`,
  preventing metadata-only overwrites from slipping through ([#60]).
- `memory_write_page` calls that specify `project` without `workspace` now
  default to the active workspace published by hooks, and project-only reads use
  the same active-workspace resolution so the write can be read back without an
  explicit workspace ([#61]).
- `memory_read_page` now accepts explicit `workspace` + `project` for sibling
  projects and falls back to the stored DB body only when the markdown file is
  missing, not when the disk source of truth is corrupt or unreadable ([#63]).
- `openai-oauth` now speaks the current ChatGPT/Codex responses stream format
  for bootstrap/consolidation requests and avoids sending the unsupported
  `max_output_tokens` field on that endpoint ([#64]).
- `ai-memory write-page` now resolves an omitted `--project` through the same
  current-project heuristic as `read-page` and `search`, preventing writes from
  landing in `scratch` while the read-back targets the cwd-derived project
  ([#66]).

## [0.8.1] - 2026-05-30

### Fixed
- **Consolidation no longer fails on long sessions** (~5,000+ observations or
  multi-hour agent runs). Two bugs surfaced trying to consolidate a real
  16-hour / 7,234-observation session:
  - **Prompt confusion (regression from the v0.8 `slot_kind` work):** the
    multi-page consolidator prompt listed `slot_kind` values
    (`state` / `invariant`) immediately above the `tier` values
    (`working` / `episodic` / `semantic` / `procedural`). The LLM read them
    as one list and emitted `tier: "state"` in structured responses, which
    deserialisation rejected. Prompt now leads with `tier` (with explicit
    "EXACTLY ONE OF FOUR strings" emphasis), then `kind`, then `slot_kind`
    under its own clearly-scoped section that states "completely unrelated
    to tier" and "only for `_slots/*` paths."
  - **No token budget on the observation dump:** `build_request` and
    `build_batch_request_with_slots` dumped every observation into the
    prompt buffer, which exceeded the provider's 200k-token context on long
    sessions (the sabadell run produced a 235k-token request → 400 from
    the provider). New `window_observations_to_budget` walks the slice
    from most-recent backward, keeping each entry whose render cost fits
    in a 400k-char budget (~100k tokens), leaving room for the system
    prompt + schema + LLM output. When entries are skipped, a prepended
    note tells the LLM the context is partial so its summary doesn't
    pretend to cover the early session. Both `PreCompact` and
    `memory_consolidate` triggers benefit from the fix — both were silently
    failing into the `warn!()` catchall on sessions this long.
  - 5 unit tests guard the windowing invariants (empty input, fits-under-
    budget passthrough, most-recent-preserved, single-too-large-obs drops
    everything, observation-boundary alignment). No schema change, no
    config knob, backward-compatible for sessions that already fit.

## [0.8.0] - 2026-05-30

### Added
- **Multi-user attribution (v0.8 Phase 1, rolling out across milestones
  P1.1–P1.8).** ai-memory's data model stays single-tenant — every
  authenticated request sees every page — but writes can now be
  attributed to a named user. Five `ai-memory user` subcommands
  (`add`, `list`, `expire`, `revive`, `rotate-token`) manage a `users`
  table; the auth middleware resolves every request to one of four
  tiers (Anonymous, Root, DB user, 401), injects an
  `Extension<ActorContext>` + `Extension<AuthLevel>` for downstream
  consumers, and gates the root-only admin user-management endpoints.
  Tokens are 32 bytes of OS CSPRNG, stored only as
  `SHA-256(token || ":" || token_pepper)` (per-server pepper from
  `[auth].token_pepper`, auto-generated on `ai-memory init`); see
  [`docs/users.md`](docs/users.md) for the SHA-256-not-argon2id
  rationale, the four-rung auth ladder, and the backward-compat
  migration for pre-v0.8 installs. New v0.8 fields on `[auth]`:
  `root_username` / `root_email` / `root_name` (label for the bearer
  token's writes) and `token_pepper`. Per-page `author_id` + web UI
  surfacing lands in P1.6/P1.7; this milestone set ships P1.1
  (`ActorContext` + `UserId` in core), P1.2 (table + writer/reader
  ops + V14 migration), P1.3 (auth middleware), P1.4 (root-gated
  `POST/GET /admin/users` + `…/expire|revive|rotate-token`), and P1.5
  (CLI subcommands). **No behaviour change for existing single-user
  installs**: without `[auth].token_pepper` the multi-user lookup
  stays dormant, user-management endpoints 503 with a clear
  `multi-user not enabled` message pointing at `ai-memory init`,
  and the existing `bearer_token`-only flow keeps authenticating
  exactly as before.
- **`memory_query { global: true }` — cross-project global search** that
  reaches every project in every workspace in one call, with each hit
  annotated by its workspace + project so the agent can tell where it
  came from. Use when the agent doesn't know which project holds a
  cross-cutting note (shared infra/ops, a sibling app). Mutually
  exclusive with `scopes`/`project`/`workspace`. Routing snippet +
  `MEMORY_INSTRUCTIONS` now teach both broadening modes (`scopes` for
  named siblings, `global=true` for unknown locations) and explicitly
  warn that `memory_query` returns snippets — use `memory_read_page`
  for full bodies. The prompt-surface contradiction the original PR
  shipped ("there is no global 'search everything' mode" right after
  the bullet advertising `global=true`) was caught in the post-merge
  audit and rewritten; the prompt regression test now refuses any
  variant of that legacy phrasing
  ([#56], thanks @djalmajr).
- **Cross-project wiki links + dependency graph.** Wikilinks gain an
  explicit scope qualifier: `[[project:path.md]]` for a sibling project
  in the same workspace, `[[workspace/project:path.md]]` for another
  workspace. Bare links are unchanged (resolve within the source's own
  project). `links.to_workspace` / `links.to_project` join the primary
  key so the same `to_path` can land in two different projects without
  colliding. `memory_lint` now reports dangling cross-project refs
  (typo'd project vs missing/renamed target page), `memory_briefing`
  exposes `cross_project_dependents` / `cross_project_dependencies`
  per project, and `GET /api/v1/graph` returns the resolved cross-
  project edges for a graph view. Migration V13 rebuilds the `links`
  table preserving existing rows as `(to_workspace=NULL,
  to_project=NULL)` — same "local" semantics as before
  ([#57], thanks @djalmajr).

### Changed
- **FTS5 queries OR-join bare multi-word inputs** instead of the
  pre-existing AND default. A natural-language query like
  `"have we discussed cross project search strategy"` previously
  required every word to co-occur in one page — near-zero recall for
  multi-word queries, which the caller silently mistook for "never
  recorded". OR + BM25 ranking (callers already `ORDER BY rank`) keeps
  the best-matching pages at the top of the list, so the user-visible
  top-N is still AND-ish; OR just adds a relevant tail instead of
  returning nothing. Explicit FTS5 syntax (`OR`/`AND`/`NOT`/`NEAR`,
  quoted phrases, parens) is detected and preserved verbatim so the
  exact-match escape hatch stays available. 5 new unit tests guard the
  preservation contract (post-merge audit). Migration V12 rebuilds the
  FTS tables with `unicode61 remove_diacritics 2` so accent-free
  Portuguese queries (`"descricao da sessao"`) match accented stored
  text (`"descrição da sessão"`); contentless FTS — source rows
  untouched ([#58], thanks @djalmajr).
- **MCP write tools now honour the session's project (and create
  named projects on demand).** Three correctness fixes on
  `memory_write_page` / `memory_lint` / `memory_forget_sweep`:
  - A `memory_write_page { project: "X" }` for a project name that
    doesn't exist used to silently fall through to the session's
    active project (find-only resolution); writes meant for a fresh
    project polluted the current one. A new `write_target_ids`
    helper uses **get-or-create** for an explicit project name, so
    a named write always lands where the agent asked.
  - `memory_lint` + `memory_forget_sweep` previously always targeted
    the server's baked `--project` regardless of the session, so a
    cross-project lint or retention sweep could never reach the
    project the user was actually working in. Both now resolve
    through the same find-only `effective_ids_for_read_args` path
    the read tools use, with the hook-published active project as
    the fallback.
  - Both `lint` / `sweep` and the new `write_page` add explicit
    `workspace` + `project` args (defaulted to current session,
    documented with the v0.5.2 "**Omit unless the user explicitly
    names a *different* project.**" tail). 2 regression tests cover
    "Bug B" (explicit-project write must create + land) and
    "Bug C" (sweep must evaluate the named project, not the baked
    default) ([#59], thanks @djalmajr).

## [0.7.1] - 2026-05-29

### Fixed
- **`install-hooks --agent codex` no longer panics with `index not found`**
  when `~/.codex/config.toml` carries an `[mcp_servers]` table that has other
  MCP servers (context7, node_repl, …) but no `ai-memory` entry — a
  perfectly valid setup since ai-memory can integrate via hooks alone.
  `infer_codex_mcp_config` used `toml_edit`'s panicking `Index` impl with
  bare `[]` chains; it now walks the table via `.get()` and returns `None`
  on any missing key. Mirrors the safe pattern the JSON variant has used
  all along. Adds 4 regression tests covering missing-entry,
  missing-table, empty-doc, and bare-entry inputs
  ([#53], thanks @Otavio-Machado-Santos).
- **`install-hooks --agent claude-code` no longer silently stages 0 scripts
  and points `settings.json` at an empty directory.** On macOS — and any
  install where the binary lives outside the repo and the system package
  paths (`/usr/local/share`, `/usr/share`) are absent — `resolve_hooks_dir`
  fell through to the data-local candidate, which was *also* the staging
  destination. The wipe-then-copy flow inside `stage_hook_scripts_in` then
  deleted the very scripts it was about to read, leaving 0 copied; the
  caller proceeded to rewrite `settings.json` anyway, disabling capture
  with no error. The function now (a) canonicalizes source and destination
  paths, skips the wipe + copy when they match and verifies in-place,
  preserving any scripts a prior `setup-agent` run extracted there, and
  (b) bails with an actionable error pointing at `--hooks-dir` or
  `ai-memory setup-agent` whenever zero scripts are present in either
  branch. Adds 3 regression tests
  ([#52], thanks @Otavio-Machado-Santos).
- **macOS thin-client wrapper no longer crashes with "Permission denied" in
  the log file appender.** The `bin/ai-memory` wrapper passed
  `-u $(id -u):$(id -g)` to the one-shot helper container, which on macOS
  collides with the data volume owner (uid 1000 inside the container vs
  uid 501/502 on the host). The wrapper now skips `-u` on Darwin so the
  container runs as its default uid 1000 — Docker Desktop's file-sharing
  layer handles host ownership transparently — while Linux and other
  Unix systems continue to receive `-u`. Same change also hardens the
  `${TTY_ARGS[@]}` / `${NETWORK_ARGS[@]}` / `${ENV_ARGS[@]}` /
  `${USER_ARGS[@]}` expansions for `set -u` compatibility on macOS's
  default bash 3.2 ([#51], thanks @abnersajr; supersedes [#50]).

## [0.7.0] - 2026-05-29

### Added
- **`memory_read_page` MCP tool** (`read-only`) for fetching the FULL body of a
  wiki page — pass `path` for a direct lookup or `query` to fetch the top FTS5
  hit's full body. Complements `memory_query`'s 24-word snippets when an agent
  needs to read an entire decision page end-to-end. Also exposed as
  `GET /admin/read-page?workspace=…&project=…&path=…` (admin HTTP) and the new
  `ai-memory read-page` CLI subcommand (thin HTTP client). All three surfaces
  scope to the current project by default and route user-supplied paths through
  `PagePath::new`, so traversal attempts (`../etc/passwd`) are rejected with
  400. ARCHITECTURE.md's MCP-tool table grows from 12 to 13 rows ([#49]).
- `_slots/*.md` pages can now declare `slot_kind: state` or
  `slot_kind: invariant` frontmatter. `state` remains the default for existing
  slots; `invariant` marks high-resistance project context or preferences that
  consolidation should not rewrite unless observations directly contradict the
  existing slot content ([#47], closes [#14]).

### Fixed
- **Windows PowerShell hooks no longer hang or stall the agent.** The shared
  `hooks/lib/ai-memory-hook.ps1` read stdin via `[Console]::In.ReadToEnd()`,
  which blocks indefinitely when the agent does not close the stdin pipe
  (observed on Claude Code `PreCompact`); because the `Invoke-WebRequest`
  timeout only starts after the read returns, a stuck read meant the hook
  never POSTed anything. Stdin is now read asynchronously, guarded by
  `[Console]::IsInputRedirected` with a 2s cap, so the hook can never freeze.
  HTTP timeouts were also raised from 1s to 3s (POST) / 2s (handoff GET) to
  tolerate remote servers over higher-latency links. The full raw payload is
  still forwarded (parity with `_lib.sh`), so observation title/body stay
  intact. Affects every agent still on the PowerShell hook runner
  (Codex, Cursor, Gemini CLI, Antigravity, OpenCode on Windows) ([#48]).
- Page upserts now treat frontmatter/title/tier/pinned changes as real page
  updates instead of short-circuiting solely on unchanged body text, keeping
  the SQLite index consistent with markdown frontmatter-only edits ([#47]).

## [0.6.1] - 2026-05-28

### Added
- `Cache-Control: private, max-age=N` headers on all `/api/v1` read endpoints
  (lists/search/recent/briefing/overview: 30–60s; single-page reads: 300s).
  Errors stay uncached. A polling SPA no longer hits the DB on every request.
- **ETag + conditional GET** on the single-page read endpoint
  (`GET /api/v1/workspaces/{ws}/projects/{p}/pages/{*path}`): the response
  carries `ETag: "<sha256>"` over the markdown body, and a follow-up request
  with matching `If-None-Match` returns `304 Not Modified` with no body.
- **`--cors-allow-origin`** flag (repeatable) and
  `AI_MEMORY_CORS_ALLOW_ORIGINS=a,b,c` env var. When set, a `CorsLayer` is
  attached **only to `/api/v1`** (`/mcp`, `/hook`, `/admin`, and `/web` are
  intentionally untouched) so a separately-hosted SPA can call the API. Each
  origin must include a scheme; `*` is rejected at startup (CORS spec forbids
  credentials + wildcard). Empty list = same-origin only, unchanged behaviour.

## [0.6.0] - 2026-05-28

### Added
- Read-only **`/api/v1`** JSON surface for third-party frontends: workspaces,
  projects, pages (list + read with frontmatter, body, resolved links, and
  back-links), recent, briefing, search (GET single/global + POST multi-scope
  capped at 25 scopes), and workspace/project `overview` aggregates (handoff +
  briefing + memory-health drill-down). Mounted before the bearer +
  host-allowlist middleware so existing auth applies automatically. Read-only
  by construction — zero writer calls in the handlers ([#7]).
- **`--web-ui-dir`** flag on `ai-memory serve` to host any static SPA at
  `/web` (same origin as the API, behind the same auth), with `index.html`
  SPA fallback via `tower-http::ServeDir`. Validates the directory exists
  and contains `index.html` before binding. When the flag is absent, the
  built-in server-side `/web` browser stays the default ([#7]).
- MCP read tools (`memory_query`, `memory_recent`, `memory_status`,
  `memory_briefing`, `memory_explore`) accept optional `workspace` +
  `scopes` args for explicit multi-project queries; existing single-`project`
  behaviour is unchanged and remains the default ([#7]).
- New reader queries powering the API: per-page outgoing links + incoming
  back-links, workspace-aggregated briefing, memory-health (stale /
  duplicate / orphan) counts and drill-down lists, workspace summaries
  with last-update timestamps ([#7]).

### Fixed
- Antigravity `pre-tool-use` hook now emits the documented
  `{"decision":"allow"}` JSON contract instead of an empty `{}`, while
  keeping the `ai_memory_post_hook` call fully suppressed
  (`>/dev/null 2>&1 || true`) so the `queued` body never bleeds into the
  hook's stdout. Identical logic for `.sh` and `.ps1`; other hook scripts
  remain silent and unchanged ([#44], thanks @ArtroxGabriel).

### Docs
- New **[`docs/frontend-api.md`](docs/frontend-api.md)** integration guide
  for `/api/v1`: auth flow, response schemas (`PageHit`, `BriefingSnapshot`,
  `HealthDetail`, `PageLinks`, …), error model, limits/pagination,
  custom-UI hosting, a worked `fetch`/`curl` example, and pointers to the
  canonical source-of-truth files.

## [0.5.2] - 2026-05-28
### Added
- `ai-memory status` / `status --json` now includes passive process-scoped LLM
  and embedding provider health based on the last real provider call, without
  active probing or token spend ([#46]).

### Changed
- Agent-facing prompts (`MEMORY_INSTRUCTIONS`, the `CLAUDE.md`/`AGENTS.md`
  routing snippet, and the per-tool `project`/`cwd` arg docstrings) now lead
  with a clear "default to the current project — do not pass `project` or
  `cwd` args unless the user names a *different* project" rule, plus a
  reminder that the SessionStart auto-fetched handoff block already covers the
  current project. Reduces cross-agent friction where a fresh agent surfaced
  the wrong project's handoff because the LLM over-eagerly passed scoping
  args. Doc-only, no behaviour change.

### Fixed
- Claude Code hook installs on native Windows now render Git Bash-compatible
  `bash -c` commands that keep the POSIX `.sh` hook scripts and convert
  drive-letter paths to Git Bash paths, matching Claude Code's actual hook
  runner instead of emitting PowerShell commands ([#45]).
- `ai-memory llm-test --provider anthropic-oauth` now parses and maps to the
  Anthropic OAuth provider instead of being rejected by clap ([#43]).

## [0.5.1] - 2026-05-27
### Changed
- Docker release publishing now builds Linux x86_64 and aarch64 artifacts once,
  reuses those artifacts for Docker images, and smoke-tests both amd64 and arm64
  images after assembling the multi-arch manifest.
- The AUR `ai-memory-bin` package now supports aarch64 using the prebuilt Linux
  aarch64 release artifact.
- Docker source builds now use the vendored Tailwind CSS artifact, avoiding
  cross-architecture Tailwind CLI cache collisions during multi-arch releases.

## [0.5.0] - 2026-05-27
### Fixed
- Docker release images now publish both `linux/amd64` and `linux/arm64`
  manifests, so Apple Silicon and ARM64 Linux hosts can pull the image without
  forcing x86 emulation ([#41]).

## [0.4.0] - 2026-05-27
### Added
- `anthropic-oauth` LLM provider: use a Claude Pro/Max subscription via
  `claude setup-token` instead of an API key. In-Rust, reuses the existing
  Anthropic Messages client (incl. structured output). **Unofficial and
  against Anthropic's usage policies — use at your own risk** (docs warn
  prominently).
- Opt-in `AI_MEMORY_CONSOLIDATE_ON_SESSION_END`: when set and an LLM provider
  is configured, SessionEnd additionally runs LLM consolidation on top of the
  always-written rule-based summary page (non-fatal on failure) ([#40]).

### Changed
- Docs recommend a small/fast model (Haiku/mini class) for the OAuth /
  subscription LLM backends — consolidation/lint/explore is summarisation, not
  hard reasoning, and small models are far easier on subscription rate limits.
- Aligned every prompt surface + doc with actual SessionEnd behavior: it always
  writes a rule-based summary page + handoff; LLM consolidation runs on
  PreCompact, on demand via `memory_consolidate`, and at session end only
  behind the new opt-in flag ([#40]).

### Fixed
- Windows own-write detection: `inode_of` now returns the real NTFS file index
  (was always `0`, which collapsed the watcher's own-write set) ([#37]).
- `ai-memory upgrade` no longer fails with `invalid value 'lib' for --agent` —
  the hook-refresh loop skips the shared `lib/` helper dir ([#38]).
- Native packaging CI now supports non-root runners whose `systemd-tmpfiles`
  lacks `--dry-run`, while still operating only inside a temporary alternate
  root.

## [0.3.2] - 2026-05-27
### Fixed
- AUR release publishing now runs with `HOME=/home/aurbuild` and an explicit
  `GIT_SSH_COMMAND`, so the workflow uses the configured AUR deploy key.

## [0.3.1] - 2026-05-27
### Changed
- Reissued the release after the initial AUR publish failure. This release was
  superseded by 0.3.2 for the AUR SSH home fix.

## [0.3.0] - 2026-05-27
### Added
- Arch Linux native packaging assets: source and prebuilt AUR package
  definitions, system/user systemd units, sysusers/tmpfiles entries, native
  config/env templates, CI-safe alternate-root packaging checks, and a manual
  disposable-distrobox integration harness for validating real service startup
  before publishing.
- Tag-triggered release automation now validates that `vX.Y.Z` matches
  `Cargo.toml`, publishes a native Linux release tarball, keeps Docker image
  publishing behind Docker Hub secrets, and optionally publishes both AUR
  package bases when `AUR_SSH_PRIVATE_KEY` is configured.
- `memory_write_page` MCP tool for explicit durable annotations, so agents can
  write permanent wiki knowledge without abusing single-use handoffs.
- `openai-oauth` LLM provider for ChatGPT/Codex accounts, including
  `ai-memory auth login|logout|status` device-flow commands and token storage
  in `<data_dir>/auth.json`.
- `copilot` LLM provider for GitHub Copilot Chat accounts. It stores a GitHub
  token via `ai-memory auth login copilot`, exchanges it for a short-lived
  Copilot API token, and sends Copilot Chat requests with `vscode-chat`
  integration headers.

### Fixed
- `install-mcp`, `install-hooks`, and `setup-agent` now honor configured
  `AI_MEMORY_SERVER_URL` defaults; `install-hooks` also reuses an existing
  ai-memory MCP entry when present, preventing remote MCP setups from
  regenerating loopback-only lifecycle hooks during installs/upgrades.
- Filesystem watcher now reindexes a project when backends report only a
  parent-directory event, improving external editor capture on macOS/FSEvents.
- OpenAI strict structured-output schema normalization now strips generated
  `$ref` annotation siblings and rewrites generated enum `oneOf` schemas to
  `anyOf`, unblocking `memory_consolidate multi_page=true` on OpenAI models.
- OpenAI-compatible embedding calls now truncate oversized page bodies, surface
  provider errors returned in HTTP 200 bodies, retry bounded HTTP 429 responses,
  and may reuse `LLM_API_KEY` when a custom embedding base URL is configured.
- `ai-memory embed --force` without `--project` now re-embeds every project in
  the workspace and purges stale/superseded embedding rows in the same scope.
- Windows hook `cwd` values sent to a Linux server now resolve projects by the
  final path component instead of treating the full backslash path as the
  project name.

## [0.2.0] - 2026-05-26
### Added
- `ai-memory bootstrap` now prunes collected sources before POSTing to the
  server and supports `--chunk-input-tokens` to process large repositories via
  sequential LLM calls instead of one oversized prompt.
- Opt-in extension event metadata for `/hook`: custom integrations can
  pass `extension=<namespace>` (and optionally `source_event=<name>`) to
  preserve a validated third-party source event while storage keeps the
  canonical `ObservationKind` closed. Unknown events without an extension
  still collapse to `other` with no source-event metadata.
- `.ai-memory.toml` marker file lets a directory tree declare its
  `workspace` (required) and `project` (optional) without depending on
  `basename($cwd)`. Lifecycle hook scripts walk up from `cwd` to find
  the closest marker and forward `cwd` plus the declared names as
  query params on `POST /hook` and `GET /handoff`. Markers can also set
  `project_strategy = "repo-root"` to derive project identity from the
  main git repository root, so linked worktrees share one project. Server
  accepts the new params as optional overrides;
  absent marker means the previous behaviour (`workspace = "default"`,
  `project = basename(cwd)`) — fully backward compatible. See
  [`docs/marker-file.md`](docs/marker-file.md).
- Oh My Pi / OMP is now a first-class integration: `install-mcp --client pi`
  and `--client omp` write native `~/.omp/agent/mcp.json` config, while
  `install-hooks --agent omp` and `--agent pi` write the TypeScript extension
  used for lifecycle capture and handoff injection.
- Graph-aware retrieval: `memory_query` now combines FTS5, wikilink-neighbor
  expansion, optional vector RRF, and bounded raw-observation fallback.
- Observation FTS indexing and unresolved-link diagnostics surfaced through
  admin/CLI status paths.
- `_slots/` wiki pages are automatically pinned and surfaced in briefing /
  explore snapshots.
- Server-side scheduled maintenance for forget sweep and lint, with optional
  embedding backfill scheduling.
- Experimental native Windows support: PowerShell Docker wrapper,
  `ai-memory.cmd`, `.ps1` lifecycle hooks in parity with `.sh` hooks, Windows
  Tailwind hash/download support, and [`docs/windows.md`](docs/windows.md).
- Google Gemini LLM provider via `AI_MEMORY_LLM_PROVIDER=gemini`, with
  `gemini-2.5-flash` as the default hosted Google model and `GEMINI_API_KEY`
  / `GOOGLE_API_KEY` support.
- Google Gemini embeddings via `AI_MEMORY_EMBEDDING_PROVIDER=google` or
  `gemini`, with `gemini-embedding-001` as the default embedding model and
  `GEMINI_API_KEY` / `GOOGLE_API_KEY` support.
- Antigravity CLI (`agy`) support for MCP config (`serverUrl`) and lifecycle
  capture through its `PreInvocation`, `PreToolUse`, `PostToolUse`, and `Stop`
  hook events.
- README support matrix for operating systems, agent integrations, LLM
  providers, and embedding providers.
- `ai-memory uninstall` — removes ai-memory's hooks, MCP registration, and
  CLAUDE.md/AGENTS.md instruction block across all detected agents (dry-run by
  default; `--apply` to execute, with timestamped backups). `--purge-data`
  wipes wiki/db/raw via the reset guard. `--only hooks|mcp|instructions` to
  narrow. MCP matching is endpoint-based by default; pass `--mcp-url` when the
  server was installed with a custom endpoint and `--mcp-name` only to narrow
  removal to one matching entry. Docker/volume teardown is printed as a hint,
  not executed.

### Changed
- Same-body page upserts are now true no-ops, avoiding periodic watcher
  reconcile writes, FTS churn, and misleading recent-page timestamps.
- Graph-neighbor expansion for hybrid search now batches all seed pages into
  one SQL query instead of issuing incoming/outgoing lookups per seed.
- Embedding backfill stores embeddings in chunks instead of one writer
  command and SQLite transaction per page.
- Hook ingestion now bounds in-flight processing and returns HTTP 429 when
  saturated instead of spawning unbounded background tasks.
- Documented the vector backend policy and the measured criteria required
  before adding `sqlite-vec`.
- Clarified Gemini CLI support docs: MCP registration, lifecycle hooks,
  SessionStart handoff injection, and SessionEnd capture are now called out
  consistently across README and install guides.
- Added OpenClaw lifecycle support via a generated native plugin package and
  updated Cursor / Claude Desktop / OpenClaw support docs against current
  upstream MCP and hook documentation.
- Docker images now bundle both POSIX and PowerShell hook scripts.
- `ai-memory uninstall --purge-data` now previews the `wiki/`/`db/`/`raw/`
  wipe in dry-run (mirroring `reset`) and refuses **up front** if an
  `ai-memory` process is alive (all-or-nothing) instead of removing the
  wiring and then skipping the purge. The data wipe is now shared with
  `reset` via a single internal helper.
- `ai-memory uninstall` only deletes generated plugin/extension files after
  re-validating their ai-memory-generated content, and never treats a matching
  filename or MCP server name alone as proof of ownership.

### Fixed
- `serve` now warns and starts when stored embedding rows were created with a
  different `(provider, model, dim)` than the current config. Hybrid search
  ignores stale rows until `ai-memory embed --force` or scheduled backfill
  re-embeds them, avoiding the previous startup deadlock.
- Session capture now persists every documented agent kind (`cursor`,
  `gemini-cli`, `claude-desktop`, `openclaw`, `omp` / `pi`) instead of
  failing the `sessions.agent_kind` database CHECK for agents added after
  the initial schema.
- `memory_handoff_begin` and `memory_handoff_accept` now resolve the active
  project the same way the briefing/search tools do, so MCP handoffs land in
  the project currently reported by hooks instead of the server's baked
  default project.
- Natural-language `memory_query` text containing bare colons, such as
  `pick: handoff`, no longer trips FTS5 column syntax errors while explicit
  FTS operators like `quick OR slow` remain supported.
- Marker-file routing now reaches the generated OpenCode and OMP
  TypeScript hook integrations, not only the POSIX/PowerShell script
  hooks. POSIX helpers also preserve the outer hook `cwd` when nested
  tool payloads contain their own `cwd`, and encode `+` correctly in
  marker-derived query parameters.
- `backup --to` now streams the tarball to disk instead of buffering the full
  archive in CLI memory.
- Hyphenated FTS5 queries such as `ai-memory` are normalized safely instead of
  being parsed as column operators.
- Gemini 2.5 Flash requests disable default dynamic thinking so hidden thought
  tokens do not consume `maxOutputTokens` and truncate strict JSON responses.
- `install-mcp --client claude-code` now prints the direct-edit JSON path as
  `~/.claude.json`, matching the `--apply` path and `claude mcp add` behavior.
- Hook routing now evicts a stale project-cache entry and retries once when a
  live server sees a cached project deleted underneath it, such as after
  `purge-project`, so capture resumes without restarting the server.
- Session-start handoff hooks now include `cwd` even without a marker file, so
  default `project = basename(cwd)` projects receive pending handoffs without
  requiring `.ai-memory.toml`.
- `ai-memory uninstall` now removes only ai-memory commands from mixed nested
  hook entries, preserves third-party commands in the same matcher, and removes
  legacy Codex inline-table MCP entries.
- Generated POSIX hook commands now shell-quote script paths and env values
  with metacharacters, fixing custom hook directories containing spaces and
  preventing shell-active token/URL fragments.
- OpenClaw's generated plugin now forwards marker-file routing params just like
  the OpenCode and OMP generated integrations.
- The Linux/macOS Docker wrapper now lets thin-client commands such as
  `status` and `bootstrap` reach the local quick-start server bound on the
  host's `127.0.0.1:49374`.

## [0.1.3] - 2026-05-24

### Added
- `ai-memory lint --no-llm` (and `memory_lint` `no_llm` arg) to run only the
  rule-based lint pass while leaving the LLM enabled for `memory_explore` /
  `memory_consolidate` ([#4]).

### Fixed
- `memory_lint` LLM contradiction pass silently never contributed: the
  `LintFinding` struct expected `severity`/`message` but the prompt asked for
  `summary`/`detail`. The prompt is now aligned to the canonical shape and the
  struct tolerates both (defaults `severity`, aliases `summary`→`message`,
  captures optional `detail`) ([#4]).
- Reasoning models (MiniMax M2.7, DeepSeek, Qwen, Kimi) that emit
  `<think>…</think>` / `<analysis>…</analysis>` blocks before the JSON broke
  structured-output parsing (`key must be a string at line 1 column 2`). The
  openai-compat provider now strips reasoning blocks and surrounding markdown
  fences before extracting the JSON object, so lint / consolidate / bootstrap
  work with reasoning models ([#5]).
- openai-compat base URLs with non-`v1` version segments (e.g. Z.AI's `/v4`)
  or a full endpoint path no longer produce `…/v1/v1/…` 404s
  ([#6], thanks @lucasliet).

## [0.1.2] - 2026-05-24

### Changed
- HTTP transport now defaults to **stateless** mode (`json_response`, no
  `Mcp-Session-Id` required), so stateless MCP clients (OpenCode
  `type: "remote"`, `curl`) work without an `mcp-remote` stdio shim
  ([#3]). New `serve --transport http --http-stateful` flag restores the
  previous session+SSE behaviour for clients that need it.

## [0.1.1] - 2026-05-24

### Added
- Wiki-structure migration framework: `wiki_migrations` SQL table (V06),
  `WikiMigration` trait, migration registry, and `run_pending` runner
  invoked at server startup before the watcher starts.
- MCP read tools (`memory_query`, `memory_recent`, `memory_status`,
  `memory_briefing`, `memory_explore`) accept an optional `project`
  argument to target a specific project on a shared server.

### Fixed
- OpenCode hook events (`tool.execute.*`, `session.*`) were rejected with
  "missing session_id" because OpenCode sends `sessionID` (capital `ID`)
  and the extractor only matched `sessionId`. All spellings are now
  accepted ([#1]).
- MCP read tools were locked to the server's static `--project` (default
  `scratch`), so on a shared HTTP server they returned empty memory even
  while hooks populated the correct per-cwd project. The hook router now
  publishes the active project to a shared pointer that the read tools
  use as their default; an explicit `project` argument overrides it ([#2]).

## [0.1.0] - 2026-05-23

### Added
- Per-project UUID-namespaced wiki layout: pages live at
  `<wiki_root>/<workspace_id>/<project_id>/<page-path>`. Rename is now
  a single column update; purge is `remove_dir_all` on the project dir.
- CLI becomes a thin HTTP client: `bootstrap`, `status`, `search`,
  `reorg`, `lint`, `forget-sweep`, `embed`, `commit`, `backup`,
  `write-page` all delegate to the running server via `/admin/*` routes.
  The server is the sole writer of wiki + SQLite.
- `purge-project` command with cascade-delete indexes and per-project
  isolation guard (refuses to delete files claimed by sibling projects).
- `rename-project` command: column-only rename, no file moves.
- `memory_install_self_routing` MCP tool: installs the agent-routing
  snippet into CLAUDE.md / AGENTS.md / `.cursorrules` in one call.
- Read-only HTTP wiki browser (`/web`) with project tree, page view,
  and full-text search.
- Bearer token auth (`AI_MEMORY_AUTH_TOKEN` / `generate-auth-token`),
  Host-header allowlist, and 10 MB body cap for the HTTP server.
- `backup` / `restore` commands using `.tar.gz` archives with live-process
  guard (refuses to run if another `ai-memory` is active on the same data dir).
- Per-cwd project routing in hooks: observations route to the project
  matching the agent's working directory, not the server default.
- `opencode` / `openclaw` aliases for the OpenCode MCP client.
- Dockerised CLI wrapper (`bin/ai-memory`) with auto-restart for the
  local container and nudge for remote upgrades.
- `bootstrap` serialises parallel runs to prevent duplicate project creation
  and handles the case where the CWD has no git repo.
- Monthly log-md rotation to keep `log.md` from growing unbounded.
- `memory_consolidate` PreCompact checkpointing falls back to rule-based
  summarisation when no LLM is configured.
- `docs/lifecycle-ops.md`: safety matrix for state-touching commands
  (reset, restore, purge-project, rename-project).
- `docs/wiki-migrations.md`: when and how to write a wiki migration.

### Changed
- `bin/ai-memory` forwards `AI_MEMORY_SERVER_URL` and no longer creates
  `-w` mount-conflict directories.
- `bootstrap` resolves the repo root via `libgit2`, removing the
  `git` binary dependency.
- Admin routes consolidated: dry-run support, correct status codes,
  deduplicated handlers.
- Host-header allowlist sourced from `Config.allowed_hosts`; logged at
  startup so operators can verify the effective list.

### Fixed
- `AI_MEMORY_HOST_CWD` handling and dry-run no-project side effects.
- Web page view: strip leading H1 from body to prevent title duplication.
- `install-mcp` Codex config key was `bearer_token`, not
  `http_headers` / `headers`.
- Consolidator used server startup default project instead of the
  session's actual project.

[Unreleased]: https://github.com/akitaonrails/ai-memory/compare/v1.32.1...HEAD
[1.32.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.32.1
[1.32.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.32.0
[1.31.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.31.1
[1.31.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.31.0
[1.30.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.30.0
[1.29.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.29.0
[1.28.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.28.1
[1.28.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.28.0
[1.27.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.27.0
[1.26.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.26.1
[1.26.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.26.0
[1.25.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.25.0
[1.24.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.24.0
[1.23.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.23.0
[1.22.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.22.0
[1.21.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.21.0
[1.20.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.20.2
[1.20.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.20.1
[1.20.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.20.0
[1.19.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.19.2
[1.19.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.19.1
[1.19.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.19.0
[1.18.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.18.0
[1.17.3]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.17.3
[1.17.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.17.2
[1.17.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.17.1
[1.17.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.17.0
[1.16.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.16.0
[1.15.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.15.0
[1.14.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.14.0
[1.13.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.13.0
[1.12.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.12.0
[1.11.4]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.11.4
[1.11.3]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.11.3
[1.11.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.11.2
[1.11.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.11.1
[1.11.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.11.0
[1.10.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.10.1
[1.10.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.10.0
[1.9.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.9.1
[1.9.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.9.0
[1.8.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.8.0
[1.7.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.7.1
[1.7.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.7.0
[1.6.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.6.0
[1.5.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.5.0
[1.4.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.4.1
[1.4.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.4.0
[1.3.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.3.0
[1.2.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.2.2
[1.2.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.2.1
[1.2.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.2.0
[1.1.3]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.1.3
[1.1.2]: https://github.com/akitaonrails/ai-memory/compare/v1.1.1...v1.1.2
[1.1.1]: https://github.com/akitaonrails/ai-memory/compare/v1.1.0...v1.1.1
[1.1.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.1.0
[1.0.11]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.11
[1.0.10]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.10
[1.0.9]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.9
[1.0.8]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.8
[1.0.7]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.7
[1.0.6]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.6
[1.0.5]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.5
[1.0.4]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.4
[1.0.3]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.3
[1.0.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.2
[1.0.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.1
[1.0.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.0
[0.16.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.16.0
[0.15.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.15.0
[0.14.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.14.0
[0.13.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.13.0
[0.12.3]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.12.3
[0.12.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.12.2
[0.12.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.12.1
[0.12.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.12.0
[0.11.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.11.0
[0.10.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.10.0
[0.9.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.9.0
[0.8.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.8.1
[0.8.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.8.0
[0.7.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.7.1
[0.7.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.7.0
[0.6.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.6.1
[0.6.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.6.0
[0.5.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.5.2
[0.5.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.5.0
[0.4.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.4.0
[0.3.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.3.2
[0.3.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.3.1
[0.3.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.3.0
[0.2.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.2.0
[0.1.3]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.1.3
[0.1.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.1.2
[0.1.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.1.1
[0.1.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.1.0
