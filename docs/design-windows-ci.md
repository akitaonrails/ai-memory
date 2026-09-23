# Design: a fast local Windows test/build loop (dockur on marvin) alongside `windows-latest`

**Status: Phase 0 shipped; Phase 1 measured on marvin (see the results section at the
end).** This began as a feasibility + design study evaluating options for a fast, reliable,
mostly-local Windows test/build loop to supplement (not blindly replace) the current GitHub
`windows-latest` CI. Phase 0 (the cross-compile build gate) has since been implemented and
Phase 1 (the dockur VM) has been stood up and benchmarked; the "Phase 1 results" section at
the end records the measured numbers and the host-specific accommodations, and revises the
recommendation accordingly. The body below is the original study, preserved as the rationale.

This is a *written study*. Every recommendation is grounded in the repo's actual setup
(`.github/workflows/windows.yml`, the enumerated `#[cfg(windows)]` surface, and the
recent flake history) and in cited external sources. Fetched web content was treated as
untrusted; only facts were extracted.

## Problem

Windows is a real, shipped target — "Experimental" in the support matrix
(`docs/support-matrix.md`), but tagged releases publish `ai-memory-windows-x86_64.zip`
with `ai-memory.exe`, and the PowerShell hook/wrapper surface exists *only* for Windows.
The `#[cfg(windows)]` regression tests are the one place several Windows-only code paths
ever execute: the Linux/macOS matrix in `ci.yml` never compiles them. So the coverage is
not optional — but the way we get it hurts.

**Quantified cost (from the `windows.yml` header comment).**

- A Windows run takes **~1000s** against **~250s** for the same tests on Linux — slower
  linking, and a cache restore that is **~10x slower** over many small files.
- Every gating job in `ci.yml` finishes in **~8 minutes**. Leaving Windows *in* that
  workflow made every pull request wait **~17 minutes**, with the last ~8 spent on a job
  that was `continue-on-error` and therefore blocked nothing.
- The response was to pull Windows out into its own workflow, gated three ways:
  nightly cron (`23 4 * * *`), `workflow_dispatch`, and the `windows` **label** on a PR
  that touches platform-sensitive code. The job dropped `continue-on-error`, so those
  runs are now red-or-green rather than advisory.

**Observed pain beyond raw slowness (from git history + CHANGELOG).**

- **Cold-start flake.** `powershell_utf8`'s connect deadline had to be widened from 10s
  to **60s** because a cold PowerShell start on a loaded `windows-latest` runner (process
  spawn + JIT + dot-sourcing the hook lib) "can take well over ten seconds before the
  hook opens its connection" (commit `94057581`, PR #842). The 10s deadline was marginal
  and flaked.
- **Eval-gate timeout relaxations.** Windows needed relaxed eval-gate test timeouts and
  aligned dependency pins (`032ec2fc`), and a further `fix/windows-eval-gate-test-timeout`
  (PR #827). Timing assumptions that hold on Linux do not hold on a shared Windows runner.
- **Hook-shell friction on the runner** (#758 family): quote the PowerShell interpreter
  path (`2cd18840`), convert MSYS paths before handing them to `powershell.exe`
  (`d5afdcbf`), and give the hook bundle its own job then gate it behind the `windows`
  label (`a8405568`, `deb64063`).
- **Fork-PR gating friction.** GitHub requires manual approval ("action_required") before
  a workflow runs on a fork PR from an outside contributor. Combined with the label gate,
  a contributor's Windows-relevant change often needs a maintainer to *both* approve the
  run *and* add the `windows` label — and only then wait ~1000s. Feedback on a
  Windows-sensitive fork PR is therefore slow and manual.

The through-line: `windows-latest` is **shared, slow, and timing-noisy**, so we pay for it
with label-gating friction, hand-relaxed timeouts, and per-flake patches. The maintainer
develops on Linux and runs a homelab box, **marvin** (Linux, 32 cores, ~2.6 TB free,
KVM available), which is idle capacity that could give a Windows loop measured in *tens of
seconds of iteration* instead of a ~17-minute round trip through GitHub.

The goal of this study: a fast, reliable, mostly-local way to run the real-Windows surface
on demand (and before every release), **without** taking on the security or licensing
liabilities that the obvious "just self-host a runner" answer carries.

## The Windows-specific surface (what actually needs real Windows)

The study has to be concrete about *what* must run on native Windows versus what Linux
already covers, or the cost/benefit of every option is guesswork. Enumerated from the code:

### Only faithfully validated on real Windows

- **Drain-lock `ERROR_LOCK_VIOLATION`** — `crates/ai-memory-cli/src/commands/hook_spool.rs`
  defines `ERROR_LOCK_VIOLATION = 33` and `is_drain_lock_busy_error` treats it as
  "busy, retry." The *unit* test injects the code via `Error::from_raw_os_error(33)` and so
  runs on any platform, but whether the OS actually *returns* code 33 for a contended
  drain lock is a real-Windows fact.
- **NTFS file-index "inode" + sharing-violation retry** —
  `crates/ai-memory-wiki/src/atomic.rs` reads the NTFS file index via
  `GetFileInformationByHandle` (`winapi_util::file::information`) so the watcher can skip
  its own writes, and `persist_with_retry` absorbs a transient
  `ERROR_SHARING_VIOLATION (32)` — "the Windows-CI antivirus scenario." The retry test
  injects error 32 everywhere; the *real* NTFS file-index value and a *real* scanner-held
  sharing violation only occur on Windows.
- **libgit2 path-resolution fallback** — `crates/ai-memory-wiki/src/git.rs` carries a
  whole `#[cfg(windows)]` fallback family (`commit_all_fallback`,
  `should_try_commit_cli_fallback`, `commit_count_fallback`,
  `recent_checkpoints_fallback`, `file_at_rev_fallback`) that shells out to the `git` CLI
  when "libgit2 can fail to reopen a freshly initialised wiki repo under dot-prefixed temp
  dirs with an OS path-resolution error" (`ErrorClass::Os` + `"failed to resolve path"`).
  This branch is dead on Linux — it never compiles there — so its behavior is *only* ever
  exercised on native Windows.
- **Verbatim / UNC / drive-letter path handling** — Windows drive-letter and UNC paths are
  treated case-insensitively in scope matching (CHANGELOG: `C:\Users\…\repo` vs a
  lower-cased form), plus verbatim-path (`\\?\`) concerns in git/atomic writes and the
  `%USERPROFILE%`-then-`dirs::home_dir` HOME fallback (`Config::load`). Path *casing* and
  verbatim semantics are OS behaviors, not code we can prove correct on Linux.
- **PowerShell hook transport** — `crates/ai-memory-hooks/tests/suite/powershell_utf8.rs`
  and `powershell_home.rs` are `#![cfg(windows)]` and drive the real
  `hooks/lib/ai-memory-hook.ps1` under a native PowerShell interpreter (UTF-8 body framing,
  marker-boundary handling). This is the cold-start flake's home.
- **PowerShell wrapper `bin/ai-memory.ps1`** — the Docker-Desktop wrapper; has had
  PowerShell 5.1-specific crashes (redirected native stderr becoming a terminating error,
  fixed with a localized probe). PS 5.1 behavior is Windows-only.
- **Detached hook-drain process launch** — `hook_drain_process.rs` detaches the background
  drainer differently on Windows vs the Unix `setsid` path.
- **Assorted `#[cfg(windows)]` in** `install_hooks`, `install_mcp`, `render_shared`, `run`
  (exec-form quoting, single-command hook strings), and `packaging::slow` tests.

### Already covered on Linux (do not pay Windows time for these)

- Every **injected-error** unit test (`from_raw_os_error(32|33)`, the sharing-violation
  retry) compiles and runs on Linux and macOS today.
- The **POSIX hook bundle** (`tests/hooks/test_lib.sh`) runs across four awks on Linux in
  `ci.yml`'s `hooks-shell` job. Only the suite's *PowerShell branch* needs a native
  interpreter.
- All **path-normalization logic that is lexical** (not OS-dependent) — the code
  deliberately normalizes lexically rather than via `fs::canonicalize` (a documented
  Windows/macOS divergence), so that logic is testable on Linux.

The practical size of the real-Windows-only surface: **two Rust concerns** (file-locking /
NTFS-index / sharing-violation, and the libgit2 path-resolution fallback + verbatim/UNC
path casing) plus **the PowerShell layer** (hook transport + `bin/ai-memory.ps1` + the
PowerShell half of the shell suite). Everything else is either injected-and-Linux-covered
or lexical. That is a small, well-bounded target — which is what makes a local VM loop
worthwhile rather than over-engineered.

## Options

Setup complexity, one-time and per-run cost, resources, **Windows licensing**, **security
posture (esp. public-repo / fork-PR exposure)**, maintenance, the coverage each gives
against the surface above, and how each integrates with `windows.yml`.

### Option A — `dockur/windows` VM on marvin, for **local on-demand** runs

Run a Windows guest as a KVM/QEMU VM inside a Docker container on marvin
(`dockur/windows`), driven from the Linux host by a `bin/`-style helper: bring the VM up,
sync the repo in (bind-mounted `/shared` → drive `Z:` in the guest, or RDP/SSH), run
`cargo test --workspace --all-targets` + the PowerShell hook suite, report, tear down.
`windows-latest` stays exactly as it is (nightly + label + pre-release), catching drift.

- **Setup complexity:** Medium one-time. `dockur/windows` boots a full Windows via an
  automated unattended install; access via web viewer (port **8006**), **RDP** (3389), and
  a **`/shared`** host bind-mount that appears as **`Z:`** inside Windows. One-time
  provisioning: install the Rust 1.95 toolchain, Git, and (native) PowerShell in the guest,
  then snapshot the disk image so subsequent boots reuse it.
- **Resources on marvin:** Needs `/dev/kvm` (marvin has it). dockur defaults to 2 cores /
  4 GB / 64 GB; Win11 realistically wants ≥4 GB and several cores for a tolerable `cargo`
  build. marvin's 32 cores / ~2.6 TB free absorb this trivially, and the VM can be given
  8–16 cores to beat a shared GitHub runner on link time.
- **Cost/time:** One-time first boot is *minutes* (image pull + retail ISO download +
  unattended install). Per-run after provisioning: a warm VM plus an incremental
  `cargo test` should land far under the ~1000s `windows-latest` figure, because (a) the
  cache lives on a local disk (no ~10x slow cache *restore* over the network) and (b) we
  can throw more cores at linking. Exact numbers must be measured on marvin (see Open
  questions) — the dockur README publishes no benchmark.
- **Licensing (address honestly):** dockur downloads **retail** consumer ISOs (Win 10/11
  Pro) and installs them **unactivated**. That is **not** covered by any evaluation-license
  grant — it is the same posture as any unactivated retail install, best described as
  **gray-area / unlicensed**. The cleanly-licensed path is to point dockur at a **Windows
  Server** or **Windows Enterprise evaluation** ISO (Server eval = full-featured **180
  days**, re-armable up to 6×; client Enterprise eval = **90 days**) via `VERSION=<ISO URL>`
  or a bind-mounted `/custom.iso`, and re-provision when the window lapses. **Recommendation
  for this project: use a Windows Server (or Enterprise) evaluation ISO, not the default
  retail Win11 download**, so the local loop is licensed for testing. (Sources below.)
- **Security posture:** **Best of the VM options.** Nothing here is a CI runner reachable
  by fork PRs. It runs only when the maintainer or the agent invokes it, on a trusted
  branch, on the maintainer's own box. The public-repo / fork-PR arbitrary-code risk that
  sinks Option B **does not apply**.
- **Maintenance:** Re-provision on eval-window expiry and on toolchain bumps; keep a
  snapshot. Modest.
- **Coverage vs the surface:** **Full.** Real NTFS file-index + sharing violations, real
  libgit2 path-resolution fallback, real verbatim/UNC + drive-letter casing, real
  PowerShell transport and `bin/ai-memory.ps1` under a native interpreter. Everything the
  enumerated surface needs.
- **Integration with `windows.yml`:** **Supplement / local-only.** Keep `windows.yml`
  as-is for drift detection; the VM becomes the *fast* pre-merge and mandatory pre-release
  path (dispatch `windows.yml` on the RC SHA still happens — belt and suspenders).

### Option B — Self-hosted GitHub Actions Windows runner on marvin (dockur or dedicated VM)

Register a self-hosted `windows` runner (inside a dockur VM or a dedicated Windows VM on
marvin) and point `windows.yml`'s `runs-on` at it.

- **The disqualifying caveat:** ai-memory is a **public** repo. GitHub's own guidance is
  explicit: *"We recommend that you only use self-hosted runners with private repositories.
  This is because forks of your public repository can potentially run dangerous code on
  your self-hosted runner machine by creating a pull request that executes the code in a
  workflow."* The secure-use reference goes further: self-hosted runners "should almost
  never be used for public repositories." A fork PR is exactly the case we *want* Windows
  feedback on, and it is exactly the attack vector.
- **Mitigations exist but are load-bearing and easy to get wrong:** never run fork
  `pull_request` on the self-hosted runner (reserve it for `push`/`schedule`/
  `workflow_dispatch` on trusted refs); require approval for outside contributors; use
  **ephemeral / just-in-time runners** (at most one job, then destroyed) so one PR cannot
  poison the next; isolate via runner groups; keep no secrets on the host; treat the VM as
  disposable. Even with all of that, a single misconfiguration (or a future
  `pull_request_target`/`workflow_run` mistake) exposes marvin to arbitrary code from the
  internet.
- **Setup/maintenance:** Highest — runner registration + auto-reset ephemeral tooling +
  keeping the isolation invariants true forever, on top of the same VM provisioning as
  Option A. Licensing: same retail-vs-eval issue as Option A.
- **Coverage:** Same *full* Windows coverage as Option A (it's the same VM), but reached
  through a CI runner instead of a local invocation — which only *adds* the fork-PR risk
  without adding coverage over Option A.
- **Verdict:** **Not acceptable for this public repo.** The one thing it buys over Option A
  — "Windows results appear automatically on a fork PR" — is precisely the thing GitHub
  warns against, and Option A already gives the maintainer the fast local loop safely. The
  fork-PR *feedback* need is better served by keeping GitHub-hosted `windows-latest` (which
  GitHub licenses and isolates) for that case.

### Option C — Cross-compile from Linux (`gnu` / `msvc` via `cargo-xwin`), plus Wine for a subset

- **`cargo build --target x86_64-pc-windows-gnu`** (MinGW-w64) and
  **`x86_64-pc-windows-msvc` via `cargo-xwin`** produce Windows binaries from Linux and
  prove the code **compiles and links** for the target — this catches `#[cfg(windows)]`
  build breaks, missing imports, and ABI/link errors *cheaply, per merge, on Linux*.
- **What it does NOT do:** cross-compiling proves nothing about whether tests *pass* on
  real Windows. Running the cross-built test binaries under **Wine** is possible but
  **cannot be trusted** for this project's surface. Wine is a compatibility layer, not an
  emulator, and specifically:
  - **PowerShell** does not run reliably on Wine (64-bit PowerShell stack-overflows;
    community wrappers shim to PowerShell *Core*, not real Windows PowerShell) — so the
    entire PowerShell hook/transport surface is untestable under Wine.
  - **NTFS case-folding** — Wine uses the host FS (case-sensitive on Linux); it does not
    reproduce NTFS case-insensitive-but-preserving semantics. Our drive-letter/UNC casing
    tests would be invalid.
  - **Win32 file-locking / sharing-violation** semantics (`ERROR_LOCK_VIOLATION`,
    `ERROR_SHARING_VIOLATION`) are incomplete/best-effort in Wine — our drain-lock and
    atomic-write retry behavior cannot be validated.
  - **libgit2 verbatim/`\\?\` path handling** is a Win32 kernel path-parsing feature Wine
    does not faithfully reproduce.
- **Verdict:** Genuinely useful as a **cheap per-merge build gate** (add a Linux job that
  cross-builds the Windows target so a `#[cfg(windows)]` compile break is caught in ~8-min
  CI instead of at release). **Wine is not a runtime replacement** for any part of the
  enumerated surface — it is at best a thin, unreliable extra that must never gate
  correctness. Treat cross-compile as a supplement to Option A, and treat Wine as
  out-of-scope.

### Option D — Keep `windows-latest`, just harden (the do-less baseline)

Status quo plus more flake-hardening: keep the label gate + nightly + pre-release
dispatch, keep widening the timing tolerances (the 60s connect deadline, relaxed eval-gate
timeouts) as flakes surface.

- **Cost/complexity/security/licensing:** Zero new infrastructure; GitHub licenses and
  isolates Windows for us; no `/dev/kvm`, no VM, no fork-PR exposure.
- **Coverage:** Full real-Windows coverage (it *is* real Windows) — this is the correctness
  floor every other option is measured against.
- **What it does not fix:** the ~1000s round trip, the ~10x-slow cache restore, the
  label-gating + fork-approval friction, and the reactive per-flake patching. The
  iteration loop stays slow, and Windows-sensitive work stays painful to develop.
- **Verdict:** the honest baseline. It must **stay** (for drift + licensed fork-PR
  coverage), but on its own it does not deliver the "fast local loop" the problem asks for.

### Options summary

| Option | Setup | Per-run vs ~1000s | Resources | Licensing | Security (public repo / fork PR) | Coverage vs surface | Role vs `windows.yml` |
|---|---|---|---|---|---|---|---|
| A — dockur VM, local on-demand | Medium (1× provision + snapshot) | Much faster (local cache, more cores) — measure | `/dev/kvm`, ~8–16 cores / ≥8 GB on marvin | Gray-area if retail; **licensed if Server/Enterprise eval ISO** | **Low** — not a runner; runs only when invoked on trusted branch | **Full** | **Supplement / local-only** |
| B — self-hosted runner | Highest (runner + ephemeral + isolation) | Fast | Same as A | Same as A | **Unacceptable** — GitHub warns against on public repos; fork PRs = arbitrary code | Full (same VM) | Would replace `runs-on` — **do not** |
| C — cross-compile (+Wine) | Low (add Linux job) | N/A (build only) | Linux CI | Fine (no Windows guest) | Fine | **Build-only**; Wine runtime **untrustworthy** for this surface | **Supplement** (per-merge build gate) |
| D — harden `windows-latest` | None | ~1000s (unchanged) | GitHub-hosted | GitHub-licensed | Fine | Full | **Keep** (baseline / drift / fork-PR) |

## Recommendation

**Adopt Option A + Option C's build gate; keep Option D; reject Option B.**

1. **Primary fast loop — `dockur/windows` on marvin, local on-demand (Option A).** A
   `bin/`-style helper brings up a provisioned Windows VM, syncs the working tree in
   (bind-mount → `Z:`), runs `cargo test --workspace --all-targets` + the PowerShell hook
   suite, and reports. This is the fast pre-merge and mandatory pre-release verification
   path, run by the maintainer/agent on a trusted box. **Provision it from a Windows Server
   (or Enterprise) evaluation ISO**, not the default retail Win11 download, to keep the loop
   cleanly licensed for testing.
2. **Cheap per-merge build gate — cross-compile (Option C).** Add a Linux CI job that
   cross-builds the Windows target (`x86_64-pc-windows-gnu`, and/or `-msvc` via
   `cargo-xwin`) so a `#[cfg(windows)]` *compile* break is caught in the ~8-minute Linux
   gate instead of at release time. This does **not** replace runtime Windows testing and
   Wine is explicitly **not** used to fake it.
3. **Keep `windows-latest` label-gated + nightly + pre-release (Option D).** It remains the
   drift catcher (toolchain/dependency changes no code touches), the licensed-and-isolated
   path for **fork-PR** Windows feedback, and the mandatory green-before-tag gate on the RC
   SHA (per AGENTS.md). The local loop is *belt*; `windows.yml` stays as *suspenders*.
4. **Do NOT put a self-hosted runner on the public repo (reject Option B).** GitHub
   explicitly warns against it; fork PRs could run arbitrary code on marvin. The only thing
   it adds over Option A is automatic Windows results on fork PRs — a need already met,
   safely, by keeping `windows-latest`.

Why this shape: the real-Windows-only surface is small and bounded (file-locking / NTFS /
sharing-violation, libgit2 path fallback + verbatim/UNC casing, and the PowerShell layer),
so a **local** VM that the maintainer already has the hardware for gives full, faithful
coverage at iteration speeds `windows-latest` can't match — *without* importing the
fork-PR attack surface or making a Windows license the thing standing between a
contributor and a merge.

## Phased adoption plan

Each phase is independently shippable and reversible; nothing removes `windows.yml`.

- **Phase 0 — Cheap win, no VM.** Add a Linux CI job that cross-compiles the Windows target
  to gate `#[cfg(windows)]` compile breaks per merge (Option C build-only). Document that
  Wine is *not* used for correctness. Low risk, immediate value.
- **Phase 1 — Provision the VM manually.** Stand up `dockur/windows` on marvin from a
  **Server/Enterprise evaluation ISO**; install Rust 1.95 + Git + native PowerShell;
  snapshot. **Measure** warm `cargo test --workspace --all-targets` + PowerShell suite time
  and compare to the ~1000s `windows-latest` figure. This measurement decides whether
  Phase 2 is worth automating (it almost certainly is, given local cache + more cores).
- **Phase 2 — The `bin/` helper.** Script "up → sync tree (`/shared`→`Z:`) → `cargo test` +
  hook suite → report → down" as a `bin/`-style command, mirroring existing helpers
  (`bin/ai-memory`, `bin/deploy`, `bin/release`). Keep it read/report only; never let it
  mutate the host tree. Document it in `docs/windows.md` and reference it from AGENTS.md's
  pre-release Windows step as the *fast* pre-check that precedes the mandatory
  `windows.yml` dispatch on the RC SHA.
- **Phase 3 — Optional convenience.** A one-command "run the Windows surface before I push
  a platform-sensitive change" wrapper, and (if desired) a scheduled *local* run on marvin
  that emails/logs drift — still **not** a GitHub runner, still trigger-only, no fork
  exposure.

At every phase, `windows.yml` stays: nightly, `windows` label, `workflow_dispatch`, and
green-before-tag on the RC SHA.

## Open questions and risks

- **Measured per-run time on marvin (blocking for the value case).** The dockur README
  publishes no install-time or per-run benchmark. Phase 1's measurement is what proves the
  loop is actually faster than ~1000s. Risk: if a cold VM boot dominates, keep a warm/
  snapshotted VM so only the incremental `cargo test` is on the critical path.
- **Eval-window churn.** Server eval = 180 days (re-armable ~6×), Enterprise client eval =
  90 days. Re-provisioning is a recurring chore; a documented snapshot + rebuild runbook
  keeps it cheap. Risk if forgotten: the guest starts hourly-shutting-down mid-run.
- **Retail-ISO temptation.** dockur's *default* is a retail Win11 download (unactivated =
  gray-area). The plan must explicitly pin an eval ISO; a future contributor copying a
  dockur quickstart could silently reintroduce the retail default. Call this out in
  `docs/windows.md`.
- **PowerShell parity.** The guest must run the *Windows PowerShell 5.1* (and/or the pinned
  PowerShell the hook lib targets) that `bin/ai-memory.ps1` and the hook suite exercise —
  the 5.1-specific redirected-stderr crash we already fixed shows version matters. Ensure
  the eval edition ships the right PowerShell, or install it during provisioning.
- **Nested-virt / GPU / `/dev/kvm` availability.** Confirmed available on marvin per the
  brief, but the helper should fail loudly with a clear message if `/dev/kvm` is absent
  (e.g., run from a container without the device passed through).
- **Drift the local loop can't see.** A local snapshot pins toolchain/dependency versions;
  `windows-latest`'s nightly is what catches *upstream* drift the snapshot would hide. This
  is the reason `windows.yml` must stay, not be replaced — the two are complementary, not
  redundant.
- **Cross-compile ABI gap.** `-gnu` cross-builds catch most compile breaks, but the shipped
  release is MSVC (`ai-memory.exe`); a `-gnu`-only gate could miss an MSVC-specific break.
  If Phase 0 uses `-gnu` for simplicity, note the gap and consider `cargo-xwin` (`-msvc`)
  for fidelity.

## Sources

External sources (fetched as untrusted data; facts only):

- **dockur/windows** — image, access ports (8006 web / 3389 RDP / `/shared`→`Z:`),
  `VERSION` editions, defaults (2 cores / 4 GB / 64 GB), `/dev/kvm` requirement:
  https://github.com/dockur/windows
- **Windows licensing for VMs / testing** — Server evaluation (180 days, re-armable):
  https://www.microsoft.com/en-us/evalcenter/evaluate-windows-server-2022 ·
  https://learn.microsoft.com/en-us/answers/questions/2128668/vm-licensing-under-windows-server-evaluation ;
  Windows 11 Enterprise dev VMs (90-day eval, cannot activate):
  https://developer.microsoft.com/en-us/windows/downloads/virtual-machines/ ·
  https://www.neowin.net/news/microsoft-updates-its-free-windows-11-virtual-machines/
- **GitHub self-hosted runners on public repos (security warning + mitigations)**:
  https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/add-runners ·
  https://docs.github.com/en/actions/reference/security/secure-use
- **Wine limitations** (compatibility layer, not an emulator; PowerShell/NTFS/file-locking/
  UNC caveats):
  https://www.winehq.org/ · https://wiki.winehq.org/FAQ ·
  https://forum.winehq.org/viewtopic.php?t=37047 ·
  https://github.com/PietJankbal/powershell-wrapper-for-wine ·
  https://forum.winehq.org/viewtopic.php?t=21619
- **Cross-compiling Rust to Windows from Linux** (`gnu`, `cargo-xwin`/`-msvc`; build-only,
  Wine-runner caveats):
  https://jake-shadle.github.io/xwin/ · https://blog.logrocket.com/guide-cross-compilation-rust/ ·
  https://github.com/cross-rs/cross/issues/550

Repo grounding (this repository): `.github/workflows/windows.yml`;
`crates/ai-memory-cli/src/commands/hook_spool.rs` (`ERROR_LOCK_VIOLATION`);
`crates/ai-memory-wiki/src/atomic.rs` (NTFS file-index, sharing-violation retry);
`crates/ai-memory-wiki/src/git.rs` (`#[cfg(windows)]` libgit2 path-resolution fallbacks);
`crates/ai-memory-hooks/tests/suite/powershell_utf8.rs`, `powershell_home.rs`;
`hooks/lib/ai-memory-hook.ps1`, `bin/ai-memory.ps1`; `docs/support-matrix.md`,
`docs/windows.md`; CHANGELOG + commits `94057581` (#842), `032ec2fc`, `2cd18840`/`d5afdcbf`
(#758), and PR #827.

## Phase 1 results — measured on marvin (2026-09-22)

The study above proposed Phase 1 as "provision the VM manually and **measure**, because
the dockur README publishes no benchmark and that number decides whether Phase 2 is worth
automating." That measurement has now been done on marvin (AMD Ryzen AI Max 395, 32
cores, 62 GB, `/var` on btrfs, SELinux enforcing). It also proved Phase 0 on a real
runner. Findings, kept honest because they change the recommendation:

### Phase 0 (cross-compile gate): shipped, works

`cargo xwin build --workspace --all-targets --target x86_64-pc-windows-msvc` is green both
locally (57s warm) and on `ubuntu-latest` (9m1s cold / **3m19s warm** — under the ~8-min
gate, and parallel to the existing slowest job). It compiles and links the whole workspace
for Windows — bundled SQLite and vendored libgit2 under clang-cl, `windows-sys`/`winapi-util`,
and every `#[cfg(windows)]` **test** binary — so a Windows-only compile/link break is caught
per-merge on Linux. Landed as the `windows cross-build (msvc)` job in `ci.yml`. Gotcha for
future maintainers: the target's `rust-std` must be added to the **1.95** toolchain that
`rust-toolchain.toml` pins, not `stable`, or the build fails with "can't find crate for
`core`"; the job pins `toolchain: "1.95"` for that reason.

### Phase 1 (dockur VM): it runs the real suite green, but it is NOT a full-suite speed win

Windows Server 2025 (Evaluation, licence-clean 180-day) installs and boots to an autologon
desktop under `dockurr/windows` on marvin, and the **full suite passes green**, including
the `#[cfg(windows)]` tests executing on native Windows (PowerShell transport, NTFS atomic
writes, libgit2 path fallback). Measured, GNU toolchain (rustc 1.95 + MinGW gcc 16.1),
8 vCPU / 8 GB:

| Run | Time (CoW on) | Time (CoW off) | Notes |
|---|---|---|---|
| Cold full (`test --workspace --all-targets`, first compile) | **2408s (~40m)** | **3146s (~52m)** | 400 crates, all tests ok; cold is compile-bound, CoW makes no reliable difference (run variance) |
| Warm full (nothing changed, test execution only) | **1600s (~27m)** | **1193s (~20m)** | 0 recompiled — pure *test-execution*; CoW-off is ~25% faster here |
| Warm single crate (`test -p ai-memory-hooks`) | **73s** | — | the realistic focused-iteration cost |
| GitHub `windows-latest` full run (from the study) | **~1000s** | — | warm cache |

The decisive, unwelcome result: **the warm full suite is slower than `windows-latest`
(~1000s)** whether or not CoW is disabled, and it is dominated by *test execution*, not
compilation (0 crates recompiled). The suite is I/O-bound — SQLite, `git2`/libgit2, and
fsync-heavy atomic-write tests — and marvin's `/var` is **btrfs**, so the QEMU raw disk
image pays copy-on-write cost on every fsync. So the "iteration measured in tens of seconds"
premise does **not** hold for a full-suite run on this box; the local VM's honest value is
(a) **on-demand focused iteration** (one crate in ~1 min, no ~17-min GitHub round trip and
no fork-approval friction) and (b) full local Windows coverage before a push. It is not a
faster full gate.

The CoW optimisation was tried (fresh install onto a `chattr +C` storage dir, so the raw
image is created nodatacow): it cut the I/O-bound warm run by **~25% (1600s → 1193s)**,
confirming fsync-on-CoW was the execution bottleneck — but not enough to beat
`windows-latest`. Cold got slightly *worse* (compile-bound; within run-to-run variance).
Verdict unchanged: keep Phase 0 + `windows.yml`; the VM is a **focused-iteration / full
local coverage** tool, not a faster full gate, and does not justify Phase 2 full-suite
automation on this hardware. Measured 2026-09-22 on marvin (Server 2025 Eval, GNU
toolchain, 8 vCPU / 8 GB, Defender disabled).

### Host-specific accommodations dockur needs on marvin (none anticipated by the study)

1. **SELinux `:Z`/`:z` on every bind mount** — otherwise `/storage` is not writable.
2. **`security_opt: [label:disable]`** — SELinux blocks `/dev/net/tun`, so dockur silently
   falls back to user-mode (passt) networking, which breaks the `\\host.lan\Data` shared
   folder and custom port-forwarding. With the label disabled it gets real NAT.
3. **The `/oem` `RunOnce` auto-provision did not fire** — the VM reached an autologon
   desktop but the first-logon provisioning never ran. Driving it via the **QEMU monitor**
   (`sendkey` to type a launch command, `screendump` to read the screen — the monitor lives
   at `/dev/shm/monitor.sock` inside the container) worked reliably and is the mechanism a
   `bin/` helper should use, not `RunOnce`.
4. **Microsoft Defender must be disabled/excluded** — real-time scanning *quarantined build
   artifacts mid-build* and killed cargo (silent early exit). `Set-MpPreference
   -DisableRealtimeMonitoring $true` + exclusions for `.cargo`/`.rustup`/the build dir fixed
   it. This is the same Defender-exclusion guidance AGENTS.md already gives for Windows dev,
   here promoted from "slow" to "correctness-breaking".
5. **Isolation flakiness**: `test -p ai-memory-wiki` hung/crawled when run alone (file-watcher
   / libgit2 tests) although the same tests passed inside the `--all-targets` run — a
   reliability caveat to investigate before relying on per-crate runs for that crate.

### Revised recommendation

- **Phase 0 — ship** (done). Unambiguous win, no Windows infra.
- **Phase 1/2 — feasible, but re-measure with CoW disabled before automating.** The VM gives
  faithful full Windows coverage on demand, but is not a faster full gate on btrfs. If Phase 2
  proceeds, the `bin/` helper must: apply fixes 1–2 in compose, disable Defender (4), drive
  provisioning + runs via the QEMU monitor (3), and prefer focused per-crate invocations.
- **Option D (`windows.yml`) stays** the authoritative, licence-clean, fork-safe pre-tag gate
  regardless — the VM is *belt*, `windows.yml` remains *suspenders*.
