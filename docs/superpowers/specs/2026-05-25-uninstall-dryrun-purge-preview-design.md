# Design — dry-run purge preview + shared data-wipe helper

> Date: 2026-05-25 · Branch: `feat/uninstall-command`
> Follow-on to [`2026-05-24-uninstall-command-design.md`](2026-05-24-uninstall-command-design.md).

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
   exists in both `commands/reset.rs` and `commands/uninstall.rs`. It was
   duplicated when `--purge-data` landed (commit `3e3d44a`). The subdir list
   is a semantic contract ("what constitutes ai-memory data"); having it in
   two places risks drift on a destructive operation (e.g. a future 4th subdir
   updated in `reset` but not `uninstall`).

## Goals

- `uninstall --purge-data` dry-run previews the data wipe, mirroring `reset`'s
  per-subdir style.
- The wipe primitive lives in exactly one place, shared by `reset` and
  `uninstall`, without coupling their divergent orchestration.
- No regression to `reset` (which currently has **zero tests**).
- Coverage on touched/new code: domain (pure logic) ≥ 90%, rest ≥ 80%.

## Non-goals

- No change to `reset`'s public behavior, flags, guard semantics, or output
  wording.
- No change to `uninstall`'s guard/refusal flow or `--apply` behavior beyond
  routing the wipe through the shared helper.
- No unification of `reset`'s "would remove" wording with `uninstall`'s
  "would purge" / "✓ purged" — each command keeps its own phrasing.

## Design

### 1. New module `commands/data_purge.rs` (mute helper)

Single home for the knowledge "which subdirs are data, and how to wipe one".
**No logging, no printing, no process check** — callers own output and the
process guard. Sits alongside the existing shared command helpers
(`apply_shared.rs`, `render_shared.rs`, `purge_project.rs`).

```rust
//! Shared data-dir wipe primitive used by `reset` and `uninstall --purge-data`.
//! Mute by design: returns the affected paths; callers own logging/printing
//! and the live-process guard (invariant #9).

use std::path::{Path, PathBuf};

/// The subdirectories that constitute ai-memory's local state.
/// `logs/` is intentionally excluded and never wiped.
pub(crate) const DATA_SUBDIRS: &[&str] = &["wiki", "db", "raw"];

/// Paths that WOULD be purged (existing data subdirs), for dry-run preview.
pub(crate) fn purge_preview(data_dir: &Path) -> Vec<PathBuf> {
    DATA_SUBDIRS
        .iter()
        .map(|s| data_dir.join(s))
        .filter(|p| p.exists())
        .collect()
}

/// Wipe each existing data subdir (remove + recreate empty). Returns the
/// paths actually purged (the subset that existed). Missing subdirs are
/// skipped, not errors.
pub(crate) fn purge_data_dirs(data_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut purged = Vec::new();
    for sub in DATA_SUBDIRS {
        let path = data_dir.join(sub);
        if !path.exists() {
            continue;
        }
        std::fs::remove_dir_all(&path)?;
        std::fs::create_dir_all(&path)?;
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
- The `bail!` process guard at the top is **unchanged**.

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
  This makes the data wipe visible in the dry-run output.
- **Apply.** Replace the inline wipe loop with:
  ```rust
  } else {
      for p in data_purge::purge_data_dirs(&config.data_dir)? {
          println!("✓ purged {}", p.display());
      }
  }
  ```
  The sibling-process guard, `purge_refused` flag, and end-of-run `bail!` are
  **unchanged** — the "remove wiring, then skip purge if a process is alive"
  behavior is preserved.

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
   - guard: with a live sibling process, `reset` bails (refusal path).
2. **Unit test `data_purge`** (fails: module absent) → create helper → green:
   - `purge_data_dirs`: seed `wiki`/`db`/`raw` + `logs/` → returns the 3 paths,
     those dirs emptied, `logs/` intact; missing subdir skipped (not returned,
     no error).
   - `purge_preview`: returns only existing subdirs.
3. **Refactor** `reset.rs` and `uninstall.rs` to use the helper → step 1 + 2
   tests stay green.
4. **Integration test `uninstall --purge-data` dry-run** (fails) → implement
   preview → green: stdout contains `would purge …/wiki|db|raw` AND the seeded
   files still exist on disk afterward.

### Coverage

- Tool: **`cargo llvm-cov`** (`cargo install cargo-llvm-cov`; add to the
  CI-parity command list in docs).
- Targets for touched/new code on this branch:
  - **Domain (pure logic) ≥ 90%**: `data_purge` (helper, const, preview),
    `uninstall`'s `strip_*` functions, `build_plan`.
  - **Rest ≥ 80%**: `reset::run`, `uninstall::run`, dispatch, print paths.
- Measure with `cargo llvm-cov --lcov` and inspect per-file coverage for the
  files this change touches; add tests until thresholds are met.

## Project-rule checks

- **Invariant #9 (live-process guard before destructive op):** preserved —
  guard stays in each caller; the mute helper performs no guard itself.
- **Workflow rule #5 (test before implementation):** honored via the
  characterization-first then TDD order above.
- **Workflow rule #6 (no refactor outside the milestone):** touching
  `reset.rs` is in-scope because the duplication was *introduced by this
  feature*; consolidating it finishes the feature cleanly. Change to `reset`
  is mechanical and pinned by new characterization tests.
- **CLAUDE.md gate (rule #3):** `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` all green before commit.
```
