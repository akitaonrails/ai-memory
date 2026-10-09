# ai-memory-wikisync

**Team-wiki sync** companion for
[ai-memory](https://github.com/akitaonrails/ai-memory) (issue #986, slices
1 and 3). It keeps explicitly allowlisted page families of a running
ai-memory server in step with a directory inside a project repository, so a
team's shared memory can live as reviewable markdown in git:

- `export` copies server pages into the repository (one way).
- `sync` also sends repository edits back to the server, through the public
  `memory_write_page` MCP tool.

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
- **Local edits win until forced.** Each page is classified three ways —
  destination file, last exported state, server body. A file that
  diverged from both is reported with a diff summary and **refused**; the
  whole batch is refused, nothing is written. `--force` overwrites the
  divergent files with server content.
- **Never deletes.** Local files and server pages are never deleted,
  including brand-new local files inside an allowlisted family. Deletes
  are slice 4.
- **Untrusted content.** Page bodies are data: transported verbatim,
  never executed, never rendered, never interpreted. Paths that would
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
| deleted | present | reported; the server page is kept |
| present | deleted | reported; the file is kept |

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
- **Re-read before each write.** Right before an import the server page is
  read again and must still render to the bytes it was classified against.
  The MCP write has no compare-and-write, so a write that lands between that
  read and the import is the one race left (the case for slice 2).
- **The server's version wins after an import.** If the server stores
  something other than the file (the sanitizer redacting a secret), the file
  is rewritten with the server's rendering.
- **Deletes are not synced yet** (slice 4). Pages gone from both sides drop
  out of the state.

## Roadmap (#986)

1. Read-only export into a project repository (`export`).
2. Conditional mutation seam (compare-and-write) in core, if independently
   justified.
3. **This release — two-way sync** (`sync`): repository edits flow back
   through the public MCP write tool.
4. Deletes and conflict reporting.
5. Post-merge hook / CI integration.
