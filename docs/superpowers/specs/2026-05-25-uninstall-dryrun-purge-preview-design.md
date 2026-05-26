# Design — dry-run purge preview + shared data-wipe helper

> Date: 2026-05-25 · Branch: `feat/uninstall-command`
> Follow-on to [`2026-05-24-uninstall-command-design.md`](2026-05-24-uninstall-command-design.md).
> Revised after subagent design review (see "Review resolutions" at the end).

## Problem

Two issues surfaced while exercising `ai-memory uninstall --purge-data`:

1. **Dry-run gap.** `uninstall --purge-data` without `--apply` prints only the
   wiring removal plan and never mentions that `wiki/`/`db/`/`raw/` would be
   wiped. The most destructive part of the operation is invisible until the
   user commits with `--apply`. By contrast `reset` (without `--confirm`)
   lists `would remove <path>` for each data subdir. The behavior is safe
   (dry-run touches nothing), but the **output is asymmetric and misleading**.

2. **Duplicated wipe logic.** The data-wipe primitive — the subdir list
   `["wiki","db","raw"]` plus the `remove_dir_all` + `create_dir_all` loop —
   exists in both `commands/reset.rs` and `commands/uninstall.rs`. `reset` is
   the original (commit `5e29909`, M1-D); `uninstall` **copied** it when
   `--purge-data` landed (commit `3e3d44a`). The subdir list is a semantic
   contract ("what constitutes ai-memory data for a wipe"); having it in two
   places risks drift on a destructive operation.

## Goals

- `uninstall --purge-data` dry-run previews the data wipe, mirroring `reset`'s
  per-subdir style.
- The `reset`↔`uninstall` wipe primitive lives in exactly one place, shared by
  both, without coupling their divergent orchestration (guard, output).
- No behavioral regression to `reset` (which currently has **zero tests**),
  and no loss of `uninstall`'s per-path error context.
- `uninstall --purge-data` is **all-or-nothing**: if another `ai-memory`
  process is alive it refuses *up front*, before removing any wiring — no
  half-done state. Matches `reset`'s guard-at-top.
- Coverage on touched/new code across this branch: critical logic ≥ 90%,
  rest ≥ 80% (line coverage, local inspection — see Coverage).

## Non-goals

- No change to `reset`'s public behavior, flags, guard semantics, or output
  wording.
- `uninstall` *without* `--purge-data` stays **unguarded**: wiring removal is
  safe while the server runs (it only edits agent config files the server
  never touches), so it does not require the server stopped.
- No unification of `reset`'s "would remove" wording with `uninstall`'s
  "would purge" / "✓ purged" — each command keeps its own phrasing.
- **No unification with `init`/`restore`.** Those declare *different* subdir
  sets on purpose (`init` = `wiki,raw,db,models`; `restore` = `wiki,db`); the
  shared helper is scoped to the `reset`↔`uninstall` set `wiki,db,raw` only.
- The `remove_dir_all` → `create_dir_all` sequence is **not atomic** (a crash
  between the two leaves a deleted-but-not-recreated subdir). This is
  pre-existing behavior shared by `reset` and `restore`; making it atomic is
  out of scope.

## Design

### 1. New module `commands/data_purge.rs` (mute helper)

Single home for the `reset`↔`uninstall` knowledge "which subdirs are wiped,
and how to wipe one". **No logging, no printing, no process check** — callers
own output and the live-process guard. Returns `anyhow::Result` with per-path
context so callers keep meaningful error messages. Sits alongside the existing
shared command helpers (`apply_shared.rs`, `render_shared.rs`,
`purge_project.rs`); it stays **CLI-local** (not in `ai-memory-core`) because
the wiped set is operation-specific, not a single domain-wide constant.

```rust
//! Shared data-dir wipe primitive used by `reset` and `uninstall --purge-data`.
//! Mute by design: returns the affected paths; callers own logging/printing
//! and the live-process guard (invariant #9). The remove+recreate is not
//! atomic — pre-existing, matches reset/restore.

use std::path::{Path, PathBuf};
use anyhow::Context;

/// The subdirectories wiped by `reset` / `uninstall --purge-data`.
/// `logs/` and `models/` are intentionally excluded and never wiped.
/// NOTE: this is the reset/uninstall set only — `init` and `restore` declare
/// their own (different) sets by design; do not converge them here.
pub(crate) const WIPE_SUBDIRS: &[&str] = &["wiki", "db", "raw"];

/// Paths that WOULD be purged (existing wipe subdirs), for dry-run preview.
pub(crate) fn purge_preview(data_dir: &Path) -> Vec<PathBuf> {
    WIPE_SUBDIRS
        .iter()
        .map(|s| data_dir.join(s))
        .filter(|p| p.exists())
        .collect()
}

/// Wipe each existing wipe-subdir (remove + recreate empty). Returns the
/// paths actually purged (the subset that existed). Missing subdirs are
/// skipped, not errors. Carries per-path context on failure.
pub(crate) fn purge_data_dirs(data_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut purged = Vec::new();
    for sub in WIPE_SUBDIRS {
        let path = data_dir.join(sub);
        if !path.exists() {
            continue;
        }
        std::fs::remove_dir_all(&path)
            .with_context(|| format!("removing {}", path.display()))?;
        std::fs::create_dir_all(&path)
            .with_context(|| format!("recreating {}", path.display()))?;
        purged.push(path);
    }
    Ok(purged)
}
```

Registered with `mod data_purge;` in `commands/mod.rs`.

### 2. `reset.rs` — call the helper, keep guard + wording + tracing

- Remove the local `const SUBDIRS`.
- Dry-run branch: `for p in data_purge::purge_preview(&config.data_dir) { println!("would remove {}", p.display()); }` then keep `(dry-run; pass --confirm to wipe)`.
- Apply branch: `for p in data_purge::purge_data_dirs(&config.data_dir)? { tracing::info!(path = %p.display(), "reset"); }` then keep `tracing::info!("reset complete")`.
- The `bail!` process guard at the top is **unchanged**. (`reset::run` already
  returns `anyhow::Result`, so the helper's `anyhow::Result` slots in; reset
  *gains* per-path error context it lacked.)

### 3. `uninstall.rs` — fix the dry-run gap + use the helper

- **Dry-run fix.** In `run()`, after `print_plan(&plan)` and before the
  `if !args.apply { … return }` early exit, add:
  ```rust
  if args.purge_data {
      for p in data_purge::purge_preview(&config.data_dir) {
          println!("would purge {}", p.display());
      }
  }
  ```
- **Up-front guard (option B / all-or-nothing).** After the dry-run early
  return and before applying any wiring change, when `args.purge_data`:
  ```rust
  if args.purge_data {
      let siblings = sibling_processes();
      if !siblings.is_empty() {
          bail!(busy_message("purge data", &siblings));   // nothing removed yet
      }
  }
  ```
  This refuses the whole command before touching anything if a process is
  alive, matching `reset`'s guard-at-top. The TOCTOU window between this check
  and the wipe is the same as `reset`'s and is accepted.
- **Apply wiring, then purge** (guard already passed — no per-purge refusal):
  ```rust
  for change in &plan { apply_change(change, name, url, args.only)?; }
  if args.purge_data {
      for p in data_purge::purge_data_dirs(&config.data_dir)? {
          println!("✓ purged {}", p.display());
      }
  }
  ```
  The per-path error context is preserved (now inside the helper). The old
  **mid-flow** `sibling_processes()` check, the `purge_refused` flag, the two
  `eprintln!` warnings, and the end-of-run conditional `bail!` are **removed** —
  the up-front guard replaces them. `print_docker_hint(args.purge_data)` is
  still called (no longer gated on `purge_refused`).

### Output ordering

Dry-run with both wiring and purge:
```
would remove SessionStart, … from /…/.claude/settings.json
would remove instruction block from /…/CLAUDE.md
would purge /…/ai-memory/wiki
would purge /…/ai-memory/db
would purge /…/ai-memory/raw
(dry-run; pass --apply to remove)
```

**Accepted edge case:** when no wiring is found but `--purge-data` is set,
the dry-run prints `Nothing to remove. ai-memory wiring not found.` followed
by the `would purge …` lines. "Nothing to remove" refers to *wiring*; the
purge lines cover *data*. Technically correct; left as-is for simplicity.

## Testing

### Order (characterization-first, then TDD)

1. **Characterize `reset` against current code (must be green BEFORE refactor).**
   These test the `reset` *command* (not the helper), so they survive the
   refactor unchanged and prove observable equivalence:
   - dry-run (no `--confirm`): seed `wiki`/`db`/`raw` with files → asserts
     `would remove <path>` per subdir, prints `(dry-run; pass --confirm to wipe)`,
     and **nothing is deleted**.
   - apply (`--confirm`): asserts `wiki`/`db`/`raw` end empty (dirs remain,
     files gone), `logs/` untouched, absent subdir skipped without error.
   - **The process-guard refusal path is NOT characterized here.**
     `process_guard::sibling_processes()` inspects the live OS process table
     via `sysinfo` with no injection seam; testing the refusal deterministically
     would require spawning a real long-lived `ai-memory` process (flaky) or
     refactoring `process_guard` (forbidden by rule #6). The guard is untouched
     by this refactor, so a refactor-invariance test of it adds nothing.
2. **Unit test `data_purge`** (fails: module absent) → create helper → green:
   - `purge_data_dirs`: seed `wiki`/`db`/`raw` + `logs/` → returns the 3 paths,
     those dirs emptied, `logs/` intact; missing subdir skipped (not returned,
     no error).
   - `purge_preview`: returns only existing subdirs.
   - missing `data_dir` entirely: `purge_preview` → empty; `purge_data_dirs`
     → `Ok(vec![])`, creates nothing.
3. **Refactor** `reset.rs` and `uninstall.rs` to use the helper → step 1 + 2
   tests stay green.
4. **Integration test `uninstall --purge-data` dry-run** (fails) → implement
   preview → green: stdout contains `would purge …/wiki|db|raw` AND the seeded
   files still exist on disk afterward.
5. **Option-B guard (best-effort, `#[ignore]`).** With `--purge-data --apply`
   and a live sibling `ai-memory` process, the command bails and **no wiring is
   removed** (all-or-nothing). Like H3 this cannot be tested deterministically
   (`sysinfo` reads the live process table, no injection seam, and rule #6
   forbids refactoring `process_guard`); described as an `#[ignore]`d
   integration test that spawns a real sibling, plus manual verification. The
   happy path (no sibling) is covered by the existing round-trip + the new
   dry-run test.

Symlinked subdirs are out of scope; behavior matches existing `reset`
(pre-existing) and is not tested.

### Coverage

- Tool: **`cargo llvm-cov`** (`cargo install cargo-llvm-cov`). It is a
  **local inspection, not a CI gate** (CI runs fmt/clippy/test/deny/audit
  only); documented under a "Coverage (local)" note, NOT the "CI parity" list.
- Metric: **line coverage** via `cargo llvm-cov --lcov` (or `--summary-only`),
  inspected per file for the files this branch touches.
- Targets for touched/new code across the whole branch (per the maintainer's
  requirement — this includes already-committed uninstall code):
  - **Critical logic ≥ 90%** (criticality, not purity): `data_purge`
    (helper, const, preview) and `uninstall`'s pure `strip_*` /
    `*_is_ours` functions.
  - **Rest ≥ 80%**: `reset::run`, `uninstall::run`, `build_plan` (it does
    filesystem IO — orchestration, not pure), dispatch, print paths.
- Add tests until thresholds are met for the touched files.

## Project-rule checks

- **Invariant #9 (live-process guard before destructive op):** preserved and
  strengthened — for `uninstall --purge-data` the guard now runs **up front**
  (all-or-nothing), matching `reset`. The mute helper performs no guard itself;
  each caller owns it.
- **Invariant #10 (atomic file writes: tmp + rename + fsync):** **N/A here.**
  #10 governs *file-content* writes (config files, wiki pages, via
  `apply_shared`); this is a *directory-tree wipe*. There is no meaningful
  atomic rmdir+mkdir primitive, and `reset` (reset.rs:42-43) and `restore`
  (restore.rs:56,60) already use raw `remove_dir_all`. The wipe is
  intentionally non-atomic and matches existing behavior.
- **Invariant #16 (CLI is a thin HTTP client):** unaffected — `reset` and
  `--purge-data` are listed #16 exceptions (server-stopped lifecycle ops); the
  helper is local-FS by design and not reachable from any server-backed command.
- **Workflow rule #5 (test before implementation):** honored via the
  characterization-first then TDD order above.
- **Workflow rule #6 (no refactor outside the milestone):** touching
  `reset.rs` is in-scope because *this feature* created the drift (uninstall
  copied reset's wipe in `3e3d44a`); consolidating both call sites onto one
  primitive is the minimal way to remove that risk. `reset` is the original and
  is touched deliberately, pinned by the characterization tests written first.
- **CLAUDE.md gate (rule #3):** `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` all green before commit.

## Out-of-scope findings (logged, not addressed here)

- **`models/` is created but never wiped.** `init.rs:13` creates
  `wiki,raw,db,models`, but `reset`/`--purge-data` wipe only `wiki,db,raw` —
  `models/` survives a reset (like `logs/`). Likely intentional (downloaded
  embedding models), but undocumented. Candidate for a separate issue;
  not changed here.

## Behavior change adopted (post-review, maintainer request)

- **Option B — all-or-nothing `--purge-data`.** The previous behavior removed
  the wiring, then *skipped* the data purge (with a warning + non-zero exit) if
  an `ai-memory` process was alive, leaving a half-done state. The guard now
  moves to the **top** of the apply path: `uninstall --purge-data` refuses
  before removing anything if a process is alive. Wiring-only `uninstall`
  (no `--purge-data`) stays unguarded, since editing agent config files is safe
  while the server runs. This makes `uninstall` (disconnect integration) and
  `reset` (wipe data) consistent in their guard discipline, and clarifies the
  mental model: `uninstall` removes integration (keeps data unless
  `--purge-data`); `reset` wipes data (keeps integration).

## Review resolutions (subagent critique → disposition)

- **H1 (helper would drop uninstall's `with_context`):** accepted — helper
  returns `anyhow::Result` with per-path context (reset *gains* context).
- **H2 (#10 not addressed):** accepted — explicit N/A note added.
- **H3 (guard test infeasible):** accepted — guard refusal removed from the
  characterization set with rationale.
- **M1 (coverage scope/metric):** partially — kept the whole-branch scope (a
  maintainer requirement, not scope creep); specified line coverage + local
  (non-CI) inspection.
- **M2 (`build_plan` is not "pure"):** accepted — moved to the ≥80% bucket;
  90% bucket reframed as "critical logic", not "pure".
- **M3 (reset-rationale inverted):** accepted — corrected (reset is the
  original; uninstall copied it).
- **M4 (move const to core?):** rejected as stated — the grep found *four*
  divergent sets (`init` adds `models`, `restore` is `wiki,db` only), so the
  sets legitimately differ; helper stays CLI-local and scoped to the
  reset↔uninstall set. Surfaced the `models/` discrepancy as an out-of-scope
  finding.
- **M5 / L1 / L2 / L4:** accepted — #16 note, missing-`data_dir` test,
  symlink sentence, and llvm-cov placed under "local, not CI-gated".
