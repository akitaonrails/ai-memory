# ai-memory-wikisync

Read-only **team-wiki export** companion for
[ai-memory](https://github.com/akitaonrails/ai-memory) (issue #986,
slice 1). It pulls explicitly allowlisted page families from a running
ai-memory server through the public, read-only `/api/v1` surface into a
directory inside a project repository, so a team's shared memory can live
as reviewable markdown in git.

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
```

- `plan` never writes, not even the state file.
- `export --apply` writes/updates markdown files under `--dest` and prints
  the `git add` / `git commit` / `git push` commands you may run yourself —
  the tool never runs git, never commits, never pushes.
- `--include FAMILY` is a strict, explicit allowlist of top-level wiki
  directories (`_rules`, `decisions`, …). At least one is required and a
  bare `*` is refused: only what you name is exported.
- Auth is a bearer token via `--token` or `AI_MEMORY_AUTH_TOKEN` (plus
  `AI_MEMORY_SERVER_URL` for the server origin). Tokens are never logged
  and never written to disk.

## What it writes

Exactly the server's canonical page projection: `path`, `title`, `body`.
The page body (which carries its own `# H1` title) is transported
verbatim. The tool never forges attribution, generated, or sync
frontmatter into page files.

All local bookkeeping lives in **one** state file,
`.ai-memory-wikisync/state.json` (mode 0600, atomically replaced after
each successful write batch): per page, the SHA-256 of the bytes last
written plus the server `ETag` observed at that write. Nothing else is
stored — no tokens, no server credentials.

The state is per clone. The state directory carries a `.gitignore` of `*`,
so committing the destination never commits the state: two clones that
shared one would conflict on every merge and trust each other's baselines.
An export made before this file existed may already have committed the
state; untrack it once with
`git rm -r --cached <dest>/.ai-memory-wikisync`.

## Safety model

- **API-only.** Documented read-only `/api/v1` endpoints (incremental
  `recent` listing with cursor paging, single-page reads with
  `ETag`/`If-None-Match` revalidation; the legacy array listing is
  accepted as a fallback). No MCP, no admin routes, no writes to the
  server. `401`/`403`/`404` become clear errors; page count and body size
  are bounded per run.
- **Path safety.** Every server-reported path is validated into a
  portable shape (ASCII, no traversal, no dotfiles, no reserved Windows
  names, `.md` leaf, bounded depth/length) before it is joined onto
  `--dest`; case-fold collisions are refused; symlinked destinations,
  symlinked components and symlinked state directories are refused; files
  are replaced atomically (tmp + rename + fsync).
- **Local edits win until forced.** Each page is classified three ways —
  destination file, last exported state, server body. A file that
  diverged from both is reported with a diff summary and **refused**; the
  whole batch is refused, nothing is written. `--force` overwrites the
  divergent files with server content.
- **Never deletes.** Local files — including brand-new local files inside
  an allowlisted family — are never deleted. Deletes are slice 4.
- **Untrusted content.** Page bodies are data: transported verbatim,
  never executed, never rendered, never interpreted. Paths that would
  escape `--dest` are refused.

## Roadmap (#986)

This is slice 1 of the accepted team-wiki sync plan:

1. **This release — read-only export** into a project repository.
2. Conditional mutation seam (compare-and-write) in core, if independently
   justified.
3. Bidirectional apply (repo edits flow back through public write tools).
4. Deletes and conflict reporting.
5. Post-merge hook / CI integration.
