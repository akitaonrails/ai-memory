## What changed

<!-- One paragraph or bullet list: the observable behaviour before vs. after. -->

## Why

<!-- The motivation: bug fix, new feature, performance, correctness. Link related issue if any. -->

## Test plan

- [ ] `cargo fmt --all -- --check` passes
- [ ] `git diff --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo tf` (or `cargo test --workspace --all-targets`) passes
- [ ] Manual test: <!-- describe what you ran and what you observed -->

## Commit attribution

- [ ] I verified the name and email on every commit in this PR and corrected
      any unintended identity before requesting merge. See
      [commit attribution guidance](https://github.com/akitaonrails/ai-memory/blob/main/CONTRIBUTING.md#commit-attribution).

## Release impact

<!-- Check exactly one. This drives which release your change ships in
     (see CONTRIBUTING.md "Versioning and deprecation policy"). -->

- [ ] **Patch** — bug fix, no new surface
- [ ] **Minor** — additive: new flag/subcommand/MCP tool/config key,
      new agent harness or provider
- [ ] **Major (breaking)** — on-disk format, removed/renamed surface,
      breaking MCP schema change — called out in "What changed" above

## CHANGELOG (merge gate)

- [ ] I added a `CHANGELOG.md` `[Unreleased]` entry — **required** for any
      user-facing change: new flag / env var / endpoint / MCP tool / marker
      key, changed behaviour, or an observable bug fix. (Exempt only for
      internal refactors, dead-code removal, and test-only churn.)
- [ ] Any changed **default** (flag behaviour, config default, env var,
      response shape) is called out explicitly in "What changed" above.

Reviewers treat a missing entry as blocking — adding it up front is what
keeps your PR merging on the first pass.

## Notes for reviewers

<!-- Anything tricky, a design decision you made, or areas you'd like extra scrutiny on. -->
