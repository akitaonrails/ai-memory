# ai-memory-wikisync

Team-wiki sync companion for
[ai-memory](https://github.com/akitaonrails/ai-memory) (issue #986, slices
1, 3, 4 and 5). It keeps explicitly allowlisted page families of a running
ai-memory server in step with a directory inside a project repository, so a
team's shared memory can live as reviewable markdown in git:

- `export` copies server pages into the repository (one way).
- `sync` also sends repository edits back to the server, through the public
  `memory_write_page` MCP tool, and with `--propagate-deletes` carries
  deletes both ways through `memory_delete_page`.
- `sync --check` is a read-only CI gate, and `install-hook` runs `sync`
  after every git merge.

`sync --apply` needs ai-memory 2.7 or later: every write and delete is
conditional on the page version the plan saw, and an older server would
ignore that condition, so `sync` refuses to write to one.

It is a standalone Cargo package (own workspace, own lockfile); the root
ai-memory workspace does not build or test it:

```bash
cargo test --manifest-path companions/ai-memory-wikisync/Cargo.toml
cargo fmt --check --manifest-path companions/ai-memory-wikisync/Cargo.toml
cargo clippy --manifest-path companions/ai-memory-wikisync/Cargo.toml --all-targets -- -D warnings
```

## Usage

```bash
# Always start with a dry-run: lists create/update/unchanged, writes nothing.
ai-memory-wikisync plan \
    --server http://127.0.0.1:49374 \
    --workspace demo --project app \
    --dest ./wiki \
    --include _rules --include decisions

# Same listing, then write — export without --apply is still a dry-run.
ai-memory-wikisync export ... --apply

# Two-way: dry-run plan of imports and exports, then apply it.
ai-memory-wikisync sync ...
ai-memory-wikisync sync ... --apply
# A page changed on both sides is a conflict until a side is chosen.
ai-memory-wikisync sync ... --apply --prefer repo   # or --prefer server
# Carry deletes too (at most 10 per run unless --max-deletes says otherwise).
ai-memory-wikisync sync ... --apply --propagate-deletes
```

- `plan` never writes, not even the state file.
- `export --apply` writes/updates markdown files under `--dest` and prints
  the `git add` / `git commit` / `git push` commands you may run yourself.
  The tool never runs git, never commits, and never pushes.
- `--include FAMILY` is a strict, explicit allowlist of top-level wiki
  directories (`_rules`, `decisions`, …). At least one is required and a
  bare `*` is refused: only what you name is exported.
- Auth is a bearer token via `--token` or `AI_MEMORY_AUTH_TOKEN` (plus
  `AI_MEMORY_SERVER_URL` for the server origin). Tokens are never logged
  and never written to disk.

## What it writes

Each file is a small frontmatter, then the server body verbatim:

```markdown
---
title: "Commit rules"
tags: ["git"]
pinned: true
---
# Commit rules
...
```

Only the fields `memory_write_page` accepts and this tool round-trips appear:
`title`, `tags` (when there are any), `pinned` (when true), and `tier` (when
it is not the default `semantic`). The values are JSON strings and lists,
which are also valid YAML. A sync sends these fields back with every import,
because the tool clears whatever a write omits. Server-generated keys
(`type`, `generated`, `last_modified_by`) never reach the repository.

All local bookkeeping lives in one state file,
`.ai-memory-wikisync/state.json` (mode 0600, atomically replaced after
each successful write batch): per page, the SHA-256 of the bytes last
written plus the server `ETag` and version id observed at that write.
Nothing else is stored: no tokens and no server credentials.

The state is per clone. The state directory carries a `.gitignore` of `*`,
so committing the destination never commits the state: two clones that
shared one would conflict on every merge and trust each other's baselines.
An export made before this file existed may already have committed the
state; untrack it once with
`git rm -r --cached <dest>/.ai-memory-wikisync`.

## Safety model

This section describes `plan` and `export`; `sync` adds the rules under
[Two-way sync](#two-way-sync).

- **API-only.** Documented read-only `/api/v1` endpoints (incremental
  `recent` listing with cursor paging, single-page reads with
  `ETag`/`If-None-Match` revalidation; the legacy array listing is
  accepted as a fallback). No admin routes, and `plan` and `export` never
  write to the server. `401`/`403`/`404` become clear errors; page count
  and body size are bounded per run.
- **Path safety.** Every server-reported path is validated into a
  portable shape (ASCII, no traversal, no dotfiles, no reserved Windows
  names, `.md` leaf, bounded depth/length) before it is joined onto
  `--dest`; case-fold collisions are refused; symlinked destinations,
  symlinked components and symlinked state directories are refused; files
  are replaced atomically (tmp + rename + fsync).
- **Local edits win until forced.** Each page is compared three ways:
  destination file, last exported state, server body. A file that
  diverged from both is reported with a diff summary and refused; the
  whole batch is then refused and nothing is written. `--force` overwrites the
  divergent files with server content.
- **Never deletes.** `plan` and `export` never delete local files or server
  pages, including brand-new local files inside an allowlisted family.
  Only `sync --propagate-deletes` deletes (see below).
- **Untrusted content.** Page bodies are data. They are transported
  verbatim and never executed, rendered, or interpreted. Paths that would
  escape `--dest` are refused.

## Two-way sync

`sync` classifies every page in the allowlisted families from three
versions: the repository file, the state entry (what the last sync wrote),
and the server page rendered into file bytes.

| Repository | Server | Action |
|---|---|---|
| unchanged | changed | export the server page |
| changed | unchanged | import the file with `memory_write_page` |
| new file | no page | import (create) |
| changed | changed | conflict: nothing is written until `--prefer repo` or `--prefer server` |
| deleted | unchanged | reported; with `--propagate-deletes`, the server page is deleted |
| unchanged | deleted | reported; with `--propagate-deletes`, the file is deleted |
| deleted | changed | with `--propagate-deletes`, a conflict: `--prefer repo` deletes the page, `--prefer server` re-exports the file |
| changed | deleted | with `--propagate-deletes`, a conflict: `--prefer repo` re-creates the page, `--prefer server` deletes the file |

Safety rules:

- **Writes only through MCP.** No wiki file or SQLite access; the server's
  sanitizer, admission and attribution apply to every import.
- **Dry-run by default.** `--apply` is required, and any refused page
  (a conflict, an invalid file) blocks the whole batch.
- **Frontmatter is required to import.** A file without it would clear the
  page's title, tags and pin on the server, so it is refused.
- **Metadata the write cannot carry is protected.** A server page with any
  frontmatter key other than `title`, `tier`, `pinned`, `tags`, `type`,
  `generated` and `last_modified_by` (a consolidated page's `summary` or
  `sources`, an MCP write's `kind` or `abstract`) is never overwritten by an
  import; the refusal names the keys. Such pages still export.
- **Every write is conditional.** Each import carries the page version the
  plan classified (`expected_page_id`), or `create_only` for a new page, and
  each delete carries the version too. A page that changes on the server
  after it was classified is refused by the server, reported as "changed
  during this run", and left with its old state for the next run; the other
  pages still sync, and the run exits non-zero. A server without these
  preconditions (before ai-memory 2.7) is detected before anything is
  written and refused.
- **The server's version wins after an import.** If the server stores
  something other than the file (the sanitizer redacting a secret), the file
  is rewritten with the server's rendering.
- **Deletes are opt-in.** Without `--propagate-deletes` a delete is only
  reported, and the page keeps its state entry so the next run reports it
  again. With it, a side is deleted only if it is unchanged since the last
  sync; a delete against an edit is a conflict like any other. Pages gone
  from both sides drop out of the state.
- **Delete safety.** More than `--max-deletes` deletes (default 10) refuses
  the whole run. A pinned server page is never deleted unless `--prefer repo`
  is given. A file is deleted only through the same path checks as a write
  (no symlinked file or directory, nothing outside `--dest`) and only if it
  still holds the bytes the plan classified. The tool never runs git: it
  prints the `git rm` to stage the deletes. Apply order is imports, exports,
  server deletes, file deletes.

## CI and post-merge

`sync --check` writes nothing (no files, no state) and reports through its
exit code:

| Exit | Meaning |
|---|---|
| 0 | in sync |
| 1 | error (server unreachable or refusing the request, invalid destination, …) |
| 2 | usage error (clap) |
| 3 | drift: imports, exports or deletes are pending (unpropagated deletes count) |
| 4 | conflicts or refusals need a person; wins over 3 |

A clone without a state file (every CI checkout: the state is never
committed) compares the repository with the server directly, so a page that
differs is drift (3), not a conflict. `--check` cannot be combined with
`--apply`. See [the cookbook](../../docs/cookbook.md) for a GitHub Actions job.

`install-hook` writes a git `post-merge` hook so a pull or merge reports
(or, opt-in, applies) a sync:

```bash
ai-memory-wikisync install-hook --workspace demo --project app \
    --dest docs/wiki --include _rules --include decisions \
    [--on-merge report|apply] [--propagate-deletes] \
    [--hooks-dir DIR] [--append] [--print]
ai-memory-wikisync uninstall-hook --dest docs/wiki [--hooks-dir DIR]
```

- The hook runs `( command -v ai-memory-wikisync >/dev/null &&
  ai-memory-wikisync sync … ) || true`: a missing binary or a failed sync
  never fails the merge. It reports by default; `--on-merge apply` adds
  `--apply`. It never passes `--prefer`, and passes `--propagate-deletes`
  only if it was given at install.
- It lives between `# >>> ai-memory-wikisync >>>` and
  `# <<< ai-memory-wikisync <<<`. Re-running `install-hook` replaces that
  block; `uninstall-hook` removes only the block (and the file, if nothing
  but a shebang is left).
- The hooks directory is found by walking up from `--dest` to `.git`,
  following a worktree's `gitdir:` file and `commondir`, so one hook serves
  every worktree; `--dest` is written relative to the repository root. If
  `core.hooksPath` is set in the repository's config, the install is refused:
  pass `--hooks-dir` with that directory, or `--print` the block for your hook
  manager.
- No token is ever written (`--token` is refused; the hook reads
  `AI_MEMORY_AUTH_TOKEN` when it runs), every argument is single-quoted, and
  newlines or NUL are refused. The file is replaced atomically with mode
  0755; a symlinked hook is refused, and an existing hook without the block
  is refused unless `--append` is given and its shebang is a POSIX shell.
  The companion never runs git.

## Roadmap (#986)

1. Read-only export into a project repository (`export`).
2. Conditional mutation seam (compare-and-write) in core (ai-memory 2.7).
3. Two-way sync (`sync`): repository edits flow back through the public MCP
   write tool.
4. Deletes and conflict reporting (`--propagate-deletes`, conditional
   writes).
5. This release: post-merge hook and CI integration (`install-hook`,
   `sync --check`).
