# Uninstall dry-run purge preview + shared data-wipe helper — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `uninstall --purge-data` preview the data wipe in dry-run and refuse up front when an `ai-memory` process is alive, and route the `reset`↔`uninstall` wipe through one shared helper — pinned by characterization tests for the currently-untested `reset`.

**Architecture:** Extract a mute `commands/data_purge` helper (one `WIPE_SUBDIRS` const + `purge_preview` + `purge_data_dirs`, returning `anyhow::Result` with per-path context, no logging/printing/guard). `reset.rs` and `uninstall.rs` call it and keep their own output + guard. `uninstall --purge-data` moves its live-process guard to the top of the apply path (all-or-nothing).

**Tech Stack:** Rust 2024, `anyhow`, `tempfile` (dev), `cargo llvm-cov` (local coverage), `cargo test`.

**Spec:** `docs/superpowers/specs/2026-05-25-uninstall-dryrun-purge-preview-design.md`

---

## File structure

- **Create** `crates/ai-memory-cli/src/commands/data_purge.rs` — the mute wipe helper + its unit tests. One responsibility: "which subdirs the reset/uninstall wipe touches, and how to wipe one."
- **Modify** `crates/ai-memory-cli/src/commands/mod.rs` — register `pub mod data_purge;`.
- **Modify** `crates/ai-memory-cli/src/commands/reset.rs` — add characterization tests; drop local `const SUBDIRS`; call the helper.
- **Modify** `crates/ai-memory-cli/src/commands/uninstall.rs` — up-front purge guard (option B), call the helper, dry-run purge preview; remove `purge_refused` flow.
- **Modify** `crates/ai-memory-cli/tests/uninstall.rs` — dry-run purge preview integration test + `#[ignore]` guard test.
- **Modify** `CHANGELOG.md` — note the behavior change.

> **Environment assumption for tests:** the in-process `reset` tests and the
> `#[ignore]` guard test assume **no live `ai-memory` server** during
> `cargo test` (true in CI; if running locally with a server up, stop it).
> `process_guard::sibling_processes()` matches processes named `ai-memory`.

---

### Task 1: Characterize `reset` (pin current behavior BEFORE refactor)

These call `reset::run` in-process (no spawned `ai-memory` process → not racy) and assert filesystem effects, not stdout. They must pass against the **current** code and survive the Task 3 refactor unchanged.

**Files:**
- Modify: `crates/ai-memory-cli/src/commands/reset.rs` (append a `#[cfg(test)] mod tests`)

- [ ] **Step 1: Append the characterization tests**

At the end of `crates/ai-memory-cli/src/commands/reset.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn seed(dir: &Path) {
        for sub in ["wiki", "db", "raw"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
            fs::write(dir.join(sub).join("f.txt"), b"x").unwrap();
        }
        fs::create_dir_all(dir.join("logs")).unwrap();
        fs::write(dir.join("logs").join("app.log"), b"log").unwrap();
    }

    fn config_for(dir: &Path) -> Config {
        Config {
            data_dir: dir.to_path_buf(),
            ..Config::default()
        }
    }

    #[test]
    fn reset_dry_run_leaves_files() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path());
        run(&config_for(tmp.path()), ResetArgs { confirm: false }).unwrap();
        assert!(tmp.path().join("wiki/f.txt").exists());
        assert!(tmp.path().join("db/f.txt").exists());
        assert!(tmp.path().join("raw/f.txt").exists());
    }

    #[test]
    fn reset_apply_wipes_data_keeps_logs() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path());
        run(&config_for(tmp.path()), ResetArgs { confirm: true }).unwrap();
        for sub in ["wiki", "db", "raw"] {
            assert!(tmp.path().join(sub).is_dir(), "{sub} dir should remain");
            assert!(!tmp.path().join(sub).join("f.txt").exists(), "{sub} emptied");
        }
        assert!(tmp.path().join("logs/app.log").exists(), "logs preserved");
    }

    #[test]
    fn reset_apply_skips_absent_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("wiki")).unwrap();
        fs::write(tmp.path().join("wiki/f.txt"), b"x").unwrap();
        // db/ and raw/ intentionally absent.
        run(&config_for(tmp.path()), ResetArgs { confirm: true }).unwrap();
        assert!(!tmp.path().join("wiki/f.txt").exists());
        assert!(tmp.path().join("wiki").is_dir());
    }
}
```

- [ ] **Step 2: Run against current code — must PASS**

Run: `cargo test -p ai-memory-cli reset::tests:: -- --test-threads=1`
Expected: 3 passed. (If they fail because a real `ai-memory` server is running locally, stop it and re-run.)

- [ ] **Step 3: Commit**

```bash
git add crates/ai-memory-cli/src/commands/reset.rs
git commit -m "test(reset): characterize wipe + dry-run before refactor"
```

---

### Task 2: Create the `data_purge` helper (TDD)

**Files:**
- Create: `crates/ai-memory-cli/src/commands/data_purge.rs`
- Modify: `crates/ai-memory-cli/src/commands/mod.rs:10` (register module)

- [ ] **Step 1: Register the module**

In `crates/ai-memory-cli/src/commands/mod.rs`, add the line in alphabetical order (after `pub mod commit;`, before `pub mod embed;`):

```rust
pub mod data_purge;
```

- [ ] **Step 2: Create the file with tests + unimplemented bodies (failing)**

Create `crates/ai-memory-cli/src/commands/data_purge.rs`:

```rust
//! Shared data-dir wipe primitive used by `reset` and `uninstall --purge-data`.
//! Mute by design: returns the affected paths; callers own logging/printing and
//! the live-process guard (invariant #9). The remove+recreate is not atomic —
//! pre-existing, matches `reset`/`restore`.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// The subdirectories wiped by `reset` / `uninstall --purge-data`.
/// `logs/` and `models/` are intentionally excluded and never wiped. This is
/// the reset/uninstall set only — `init` and `restore` declare their own
/// (different) sets by design; do not converge them here.
pub(crate) const WIPE_SUBDIRS: &[&str] = &["wiki", "db", "raw"];

/// Paths that WOULD be purged (existing wipe subdirs), for dry-run preview.
pub(crate) fn purge_preview(data_dir: &Path) -> Vec<PathBuf> {
    todo!()
}

/// Wipe each existing wipe-subdir (remove + recreate empty). Returns the paths
/// actually purged (the subset that existed). Missing subdirs are skipped, not
/// errors. Carries per-path context on failure.
pub(crate) fn purge_data_dirs(data_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn seed(dir: &Path) {
        for sub in ["wiki", "db", "raw"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
            fs::write(dir.join(sub).join("f.txt"), b"x").unwrap();
        }
        fs::create_dir_all(dir.join("logs")).unwrap();
        fs::write(dir.join("logs").join("app.log"), b"log").unwrap();
    }

    #[test]
    fn purge_data_dirs_wipes_set_keeps_logs() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path());
        let purged = purge_data_dirs(tmp.path()).unwrap();
        assert_eq!(purged.len(), 3);
        for sub in ["wiki", "db", "raw"] {
            assert!(tmp.path().join(sub).is_dir());
            assert!(!tmp.path().join(sub).join("f.txt").exists());
        }
        assert!(tmp.path().join("logs/app.log").exists());
    }

    #[test]
    fn purge_data_dirs_skips_absent() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("wiki")).unwrap();
        let purged = purge_data_dirs(tmp.path()).unwrap();
        assert_eq!(purged, vec![tmp.path().join("wiki")]);
    }

    #[test]
    fn purge_missing_data_dir_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        assert!(purge_preview(&missing).is_empty());
        assert!(purge_data_dirs(&missing).unwrap().is_empty());
        assert!(!missing.exists());
    }

    #[test]
    fn purge_preview_lists_only_existing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("db")).unwrap();
        assert_eq!(purge_preview(tmp.path()), vec![tmp.path().join("db")]);
    }
}
```

- [ ] **Step 3: Run — must FAIL**

Run: `cargo test -p ai-memory-cli data_purge::`
Expected: tests fail (panic `not yet implemented` from `todo!()`).

- [ ] **Step 4: Implement the two functions**

Replace the two `todo!()` bodies:

```rust
pub(crate) fn purge_preview(data_dir: &Path) -> Vec<PathBuf> {
    WIPE_SUBDIRS
        .iter()
        .map(|s| data_dir.join(s))
        .filter(|p| p.exists())
        .collect()
}

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

- [ ] **Step 5: Run — must PASS**

Run: `cargo test -p ai-memory-cli data_purge::`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
git add crates/ai-memory-cli/src/commands/data_purge.rs crates/ai-memory-cli/src/commands/mod.rs
git commit -m "feat(cli): add mute data_purge helper (shared wipe primitive)"
```

---

### Task 3: Route `reset` through the helper

**Files:**
- Modify: `crates/ai-memory-cli/src/commands/reset.rs:1-48`

- [ ] **Step 1: Replace the imports + `run` body, drop `SUBDIRS`**

Replace everything from the top of the file through the end of `run` (the `const SUBDIRS` line and the two `for sub in SUBDIRS` loops) with:

```rust
//! `ai-memory reset --confirm` — wipe wiki/, db/, raw/ contents.
//!
//! Refuses to run while another `ai-memory` process is alive (lesson from
//! basic-memory #765, where a zombie process holding the old SQLite
//! inode caused phantom search results after a reset).

use anyhow::{Result, bail};

use crate::cli::ResetArgs;
use crate::commands::data_purge;
use crate::config::Config;
use crate::process_guard::{busy_message, sibling_processes};

/// Run the `reset` subcommand.
///
/// # Errors
/// Returns an error if another `ai-memory` process is running, if
/// `--confirm` was not provided, or if a directory cannot be removed.
pub fn run(config: &Config, args: ResetArgs) -> Result<()> {
    let siblings = sibling_processes();
    if !siblings.is_empty() {
        bail!(busy_message("reset", &siblings));
    }

    if !args.confirm {
        for path in data_purge::purge_preview(&config.data_dir) {
            println!("would remove {}", path.display());
        }
        println!("(dry-run; pass --confirm to wipe)");
        return Ok(());
    }

    for path in data_purge::purge_data_dirs(&config.data_dir)? {
        tracing::info!(path = %path.display(), "reset");
    }
    tracing::info!("reset complete");
    Ok(())
}
```

(Leave the `#[cfg(test)] mod tests` from Task 1 untouched below this.)

- [ ] **Step 2: Run reset + helper tests — must PASS unchanged**

Run: `cargo test -p ai-memory-cli reset:: data_purge:: -- --test-threads=1`
Expected: all green (Task 1's 3 + Task 2's 4). This proves the refactor is behavior-preserving.

- [ ] **Step 3: Commit**

```bash
git add crates/ai-memory-cli/src/commands/reset.rs
git commit -m "refactor(reset): use shared data_purge helper"
```

---

### Task 4: `uninstall` — up-front purge guard (option B) + helper

**Files:**
- Modify: `crates/ai-memory-cli/src/commands/uninstall.rs:14` (import) and `:203-263` (`run` apply path)

- [ ] **Step 1: Add `data_purge` to the imports**

Change line 14 of `crates/ai-memory-cli/src/commands/uninstall.rs` from:

```rust
use crate::commands::{install_hooks, install_mcp};
```

to:

```rust
use crate::commands::{data_purge, install_hooks, install_mcp};
```

- [ ] **Step 2: Rewrite the apply path of `run`**

Replace the block from `if plan.is_empty() && !args.purge_data {` (currently line 213) through the final `Ok(())` of `run` (currently line 262) with:

```rust
    if plan.is_empty() && !args.purge_data {
        return Ok(());
    }

    // All-or-nothing: when we're going to purge data, refuse before touching
    // anything if an ai-memory process is alive (matches reset's guard-at-top).
    // Wiring-only uninstall stays unguarded — it edits agent config files the
    // server never touches.
    if args.purge_data {
        let siblings = crate::process_guard::sibling_processes();
        if !siblings.is_empty() {
            anyhow::bail!(crate::process_guard::busy_message("purge data", &siblings));
        }
    }

    if std::io::stdin().is_terminal() && !args.yes {
        eprint!("Proceed with removal? [y/N] ");
        use std::io::Write as _;
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("aborted.");
            return Ok(());
        }
    }

    for change in &plan {
        apply_change(change, name, url, args.only)?;
    }

    if args.purge_data {
        for path in data_purge::purge_data_dirs(&config.data_dir)? {
            println!("✓ purged {}", path.display());
        }
    }

    print_docker_hint(args.purge_data);

    Ok(())
```

This removes the `purge_refused` flag, the mid-flow `sibling_processes()` check, the two `eprintln!` warnings, and the end-of-run conditional `bail!`.

- [ ] **Step 3: Verify the existing uninstall suite stays green**

Run: `cargo test -p ai-memory-cli --test uninstall`
Expected: the existing tests pass (none use `--purge-data`, so the apply rewrite must not regress them).

- [ ] **Step 4: Commit**

```bash
git add crates/ai-memory-cli/src/commands/uninstall.rs
git commit -m "feat(uninstall): all-or-nothing --purge-data guard + shared wipe"
```

---

### Task 5: `uninstall` dry-run purge preview (TDD)

**Files:**
- Modify: `crates/ai-memory-cli/tests/uninstall.rs` (append test)
- Modify: `crates/ai-memory-cli/src/commands/uninstall.rs:207-212` (`run` dry-run section)

- [ ] **Step 1: Append the failing integration test**

At the end of `crates/ai-memory-cli/tests/uninstall.rs`:

```rust
#[test]
fn uninstall_dry_run_previews_purge() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    for sub in ["wiki", "db", "raw"] {
        std::fs::create_dir_all(data.path().join(sub)).unwrap();
        std::fs::write(data.path().join(sub).join("f.txt"), b"x").unwrap();
    }

    let out = Command::new(bin())
        .args(["uninstall", "--purge-data"]) // dry-run: no --apply
        .env("HOME", home.path())
        .env("AI_MEMORY_DATA_DIR", data.path())
        .output()
        .unwrap();
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("would purge"), "stdout was: {stdout}");
    for sub in ["wiki", "db", "raw"] {
        let p = data.path().join(sub);
        assert!(
            stdout.contains(&p.display().to_string()),
            "missing {sub} in: {stdout}"
        );
        // Dry-run must not delete.
        assert!(p.join("f.txt").exists(), "{sub} must be untouched");
    }
}
```

- [ ] **Step 2: Run — must FAIL**

Run: `cargo test -p ai-memory-cli --test uninstall uninstall_dry_run_previews_purge`
Expected: FAIL — stdout has no `would purge` line yet.

- [ ] **Step 3: Add the dry-run preview to `run`**

In `crates/ai-memory-cli/src/commands/uninstall.rs`, change the dry-run section (currently lines 207-212) from:

```rust
    let plan = build_plan(&args)?;
    print_plan(&plan);
    if !args.apply {
        println!("(dry-run; pass --apply to remove)");
        return Ok(());
    }
```

to:

```rust
    let plan = build_plan(&args)?;
    print_plan(&plan);
    if args.purge_data {
        for path in data_purge::purge_preview(&config.data_dir) {
            println!("would purge {}", path.display());
        }
    }
    if !args.apply {
        println!("(dry-run; pass --apply to remove)");
        return Ok(());
    }
```

- [ ] **Step 4: Run — must PASS**

Run: `cargo test -p ai-memory-cli --test uninstall uninstall_dry_run_previews_purge`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-memory-cli/src/commands/uninstall.rs crates/ai-memory-cli/tests/uninstall.rs
git commit -m "feat(uninstall): preview data purge in dry-run; test"
```

---

### Task 6: Best-effort guard test (`#[ignore]`)

The guard refusal can't be tested deterministically (sysinfo reads the live process table; no injection seam; rule #6 forbids refactoring `process_guard`). This `#[ignore]`d test spawns a real sibling for manual verification.

**Files:**
- Modify: `crates/ai-memory-cli/tests/uninstall.rs` (append test)

- [ ] **Step 1: Append the ignored test**

```rust
/// Best-effort, NOT in the default run (see spec H3): the live-process guard
/// inspects the real OS process table via sysinfo with no injection seam.
/// Run with `cargo test -p ai-memory-cli --test uninstall -- --ignored`.
#[test]
#[ignore]
fn purge_data_refuses_when_sibling_alive() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    let settings = claude.join("settings.json");
    let original = r#"{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"AI_MEMORY_HOOK_URL=x /a/stop.sh"}]}]}}"#;
    std::fs::write(&settings, original).unwrap();

    // Long-lived sibling `ai-memory` process.
    let mut serve = Command::new(bin())
        .arg("serve")
        .env("HOME", home.path())
        .env("AI_MEMORY_DATA_DIR", data.path())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));

    let out = Command::new(bin())
        .args(["uninstall", "--apply", "--yes", "--purge-data"])
        .env("HOME", home.path())
        .env("AI_MEMORY_DATA_DIR", data.path())
        .output()
        .unwrap();

    serve.kill().ok();
    serve.wait().ok();

    assert!(!out.status.success(), "should refuse while a sibling is alive");
    // All-or-nothing: wiring must be untouched.
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        original,
        "no wiring should be removed when the purge is refused up front"
    );
}
```

- [ ] **Step 2: Run it explicitly to sanity-check (best-effort)**

Run: `cargo test -p ai-memory-cli --test uninstall -- --ignored purge_data_refuses_when_sibling_alive`
Expected: PASS. (If `serve` fails to bind the default port because another instance is up, free the port and retry — this is why it's `#[ignore]`.)

- [ ] **Step 3: Commit**

```bash
git add crates/ai-memory-cli/tests/uninstall.rs
git commit -m "test(uninstall): ignored best-effort guard-refusal test"
```

---

### Task 7: Coverage check, CHANGELOG, full gate

**Files:**
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Install + run coverage**

```bash
cargo install cargo-llvm-cov   # one-time; pulls llvm-tools-preview if needed
cargo llvm-cov --summary-only -p ai-memory-cli 2>/dev/null \
  | grep -E "data_purge.rs|reset.rs|uninstall.rs"
```

Expected: per-file **line** coverage — `data_purge.rs` ≥ 90% (critical logic), `reset.rs`/`uninstall.rs` ≥ 80%. If any touched file is below target, add a focused test (e.g. an extra `data_purge` edge case, or an `uninstall --only` dry-run preview assertion) until it clears, then re-run.

- [ ] **Step 2: Add CHANGELOG entry**

In `CHANGELOG.md`, under the top/unreleased section:

```markdown
### Changed
- `ai-memory uninstall --purge-data` now previews the `wiki/`/`db/`/`raw/`
  wipe in dry-run (mirroring `reset`) and refuses **up front** if an
  `ai-memory` process is alive (all-or-nothing) instead of removing the
  wiring and then skipping the purge. The data wipe is now shared with
  `reset` via a single internal helper.
```

- [ ] **Step 3: Run the full CLAUDE.md gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: no fmt diffs, no clippy warnings, all tests green.

- [ ] **Step 4: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs(changelog): uninstall dry-run purge preview + all-or-nothing guard"
```

---

## Self-review

- **Spec coverage:** dry-run preview → Task 5; shared mute helper (`WIPE_SUBDIRS`/`purge_preview`/`purge_data_dirs`, `anyhow::Result` + context) → Task 2; `reset` uses it, wording/tracing kept → Task 3; `uninstall` uses it + option B up-front guard, `purge_refused` removed → Task 4; characterization-first for untested `reset` → Task 1; coverage 90/80 line, local → Task 7; `#[ignore]` guard test per H3 → Task 6; CHANGELOG → Task 7. All spec sections map to a task.
- **Placeholder scan:** no TBD/TODO; every code step shows full code; the only `todo!()` is the intentional TDD red state in Task 2 Step 2, made green in Step 4.
- **Type consistency:** `WIPE_SUBDIRS`, `purge_preview(&Path) -> Vec<PathBuf>`, `purge_data_dirs(&Path) -> anyhow::Result<Vec<PathBuf>>` used identically in Tasks 2/3/4/5; `ResetArgs { confirm }` and `Config { data_dir, ..Default }` match the real signatures; `print_docker_hint(bool)` call updated to drop `purge_refused`.
```
