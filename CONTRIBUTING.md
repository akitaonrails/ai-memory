# Contributing to ai-memory

## Dev setup

```bash
git clone https://github.com/akitaonrails/ai-memory
cd ai-memory
cargo build --workspace
cargo test --workspace
```

Rust 1.95 is required (pinned in `rust-toolchain.toml`). The build is
self-contained: SQLite is bundled via `rusqlite`'s `bundled` feature, and
`libgit2` is vendored via `git2`'s `vendored-libgit2` feature. No system
libraries need installing beyond a standard C toolchain.

## Commit attribution

GitHub associates commits with accounts through the author email stored in
each commit. Before pushing a branch, inspect every commit that the pull request
will add:

```bash
git log --format='%h %an <%ae>' "$(git merge-base HEAD origin/main)"..HEAD
```

Use an email verified by your GitHub account, or its GitHub-provided `noreply`
address. Set it for this checkout when your global Git identity belongs to a
different project or employer:

```bash
git config --local user.name "Your Name"
git config --local user.email "your-verified-address@example.com"
```

Correct attribution mistakes on the pull-request branch before it is merged.
The project does not rewrite shared `main` history or published release tags
solely to change attribution because doing so invalidates commit hashes and
breaks existing clones and forks. Maintainers use [`.mailmap`](.mailmap) to
canonicalize accidental aliases without changing published commits.

## Required gates before every PR

All four must pass — the CI workflow enforces them and so does the `bin/release`
script:

```bash
cargo fmt --all -- --check          # formatting
cargo clippy --workspace --all-targets -- -D warnings   # lints
cargo test --workspace              # tests
cargo deny check                    # dependency policy
```

If `cargo-deny` or `cargo-audit` are not installed:

```bash
cargo install cargo-deny cargo-audit
```

## CHANGELOG is a merge gate

Every **user-facing** change must add a `CHANGELOG.md` entry under
`## [Unreleased]` in the same PR. User-facing means: a new CLI flag or
subcommand, env var, HTTP/admin endpoint, MCP tool or tool-response field,
`.ai-memory.toml` marker key, any changed behaviour or default, or an
observable bug fix. Internal refactors, dead-code removal, and test-only
churn are exempt.

This has been the single most-forgotten obligation across review batches,
so reviewers treat a missing entry as **blocking** — the PR template has a
checkbox for it. Follow the existing entry style (past-tense summary,
trailing `([#NNN])` PR/issue reference) and place it under the right
`### Added` / `### Changed` / `### Fixed` heading.

## Workflow rules (condensed from AGENTS.md)

The full authoritative rules are in [`AGENTS.md`](AGENTS.md) — the single
canonical agent/contributor rules file (`CLAUDE.md` is just a pointer to
it). Short version:

1. Work milestone by milestone. Do not start M(n+1) until every "Done when"
   bullet in M(n) passes (see `docs/design-decisions.md`).
2. No dead code, no half-built features. Stubs are documented with
   `// M<n> TODO` in the module doc-comment.
3. Write tests before claiming done. Parsers, ID derivation, and
   retention/decay math especially.
4. Do not refactor outside the milestone. Only touch what the current
   milestone requires.
5. Comments explain *why*, never *what*. No comments that restate the line
   above them.

## Cross-cutting invariants

Never violate any of the invariants in [`AGENTS.md`](AGENTS.md) (see the
"Rust Engineering Rules" and "Project Maintenance Rules" sections).
Highlights for contributors:

- All SQLite writes go through the single writer actor (`WriterHandle`).
- Config is read once at startup; never call `std::env::var` outside `Config::load`.
- Atomic file writes only: tmp + rename + fsync; never write in-place.
- Every wiki page is namespaced by `(workspace_id, project_id)`.
- The CLI is always a thin HTTP client to the running server — it never
  opens the SQLite file or the wiki directory directly.

## Versioning and deprecation policy

This project follows [Semantic Versioning](https://semver.org/):

- **Patch** (`x.y.Z`): bug fixes that do not change public API or
  on-disk format.
- **Minor** (`x.Y.0`): additive changes; new CLI subcommands, new MCP
  tools, new config keys. Existing behaviour is preserved.
- **Major** (`X.0.0`): breaking changes. This includes on-disk format
  changes that are not handled by a migration, removal of CLI subcommands,
  or changes to the MCP tool schema that would break existing agents.

Breaking changes only ship in major releases. Deprecated items are
documented in the CHANGELOG under `### Deprecated` and removed no sooner
than the following major release.
