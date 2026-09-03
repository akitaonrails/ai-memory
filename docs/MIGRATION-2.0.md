# Upgrading to 2.0

2.0 changes the wiki's on-disk format to the [Open Knowledge Format
v0.2](okf.md). The upgrade is automatic, backup-gated, and reversible.
This page describes exactly what happens and how to go back.

## What happens on the first 2.0 start

When `ai-memory serve` starts on a data directory created by 1.x, a
one-shot wiki migration runs before the server accepts any traffic:

1. **A full backup is taken first — or nothing happens at all.** The
   entire data directory (wiki files, SQLite database, config) is
   compressed to a timestamped archive in your home directory:

   ```
   ~/ai-memory-backup-okf-v0.2-<date>.tar.gz
   ```

   Set `AI_MEMORY_BACKUP_DIR=/somewhere/else` before starting if your
   home is small; the destination must be outside the data directory.
The archive is re-opened and verified after writing. **If the backup
    cannot be written or verified, the migration aborts and the server
    refuses to start** — your data is untouched and the error says why.

    The walk skips the runtime `.serve.lock` file (and its
    `.serve.lock.holder` sidecar) so the initial-start abort seen on
    Windows — where the same `serve` process holds that lock under a
    mandatory exclusive `LockFileEx`, making the backup read fail with
    `os error 33` — cannot happen. If you are on 2.0.0/2.0.1 and still
    hit it, dropping the container/process (or using a byte-range decoy
    lock above the first 64 KiB with `serve --force`) unblocks the
    one-time migration; a later patch releases these notes.

2. The wiki git history gets a `pre-okf-migration checkpoint` commit.

3. Every page's frontmatter is rewritten **in place** — same page ids,
   same version rows, bodies untouched, timestamps untouched. No page
   is superseded, embeddings stay valid, nothing looks freshly edited.

4. Every project directory gains an `index.md` declaring
   `okf_version: "0.2"` (only if one does not already exist).

5. One `okf-migration` git commit records the whole rewrite.

The migration is idempotent: restarting the server re-runs nothing and
never takes a second backup. Fresh installs skip all of it, including
the backup.

## Embeddings on by default

2.0 also turns on [local embeddings](local-embeddings.md) for installs
that never configured an embedding provider: the first start downloads
the ~87 MB model in the background (checksum-pinned, to
`<data_dir>/models/`), the next start enables hybrid search, and
existing pages are embedded automatically by a startup backfill. No
data leaves the machine — inference is in-process. Hosts that cannot
fetch the model keep the old FTS-only behaviour with a warning. Opt
out with `embedding_provider = "none"`; installs with a configured
provider are untouched.

## Running in a server or container (docker deploys)

Inside a container the home directory is **ephemeral** — it lives in
the container layer and is destroyed on the next `docker compose up
-d` recreation, which would silently lose the safety archive. The
migration detects containers (the official image's
`AI_MEMORY_IN_CONTAINER`, or `/.dockerenv` / `/run/.containerenv`) and
defaults the archive to the persistent data volume instead:

```
/data/backups/ai-memory-backup-okf-v0.2-<date>.tar.gz
```

The archive survives redeploys with the volume, and the backups
directory is excluded from the archive itself. To copy it off-host:

```bash
docker cp ai-memory:/data/backups/ai-memory-backup-okf-v0.2-<date>.tar.gz .
# or read it straight from the volume's host path
```

`AI_MEMORY_BACKUP_DIR` still wins when set (point it at another
mounted volume if you prefer). Deleting the archive — from inside or
outside the container — clears the homepage notice, same as on a
workstation.

## After the migration

The first visit to the wiki homepage opens a one-time dialog explaining
the upgrade and the recovery steps, with a "do not show me again"
checkbox (per browser; a future migration shows it again). Separately,
a banner shows the archive's location, size and date until you delete
the archive file:

- **Everything looks right?** Delete the archive; the notice disappears
  on its own.
- **Something is missing?** Restore (below).

Verify the migration if you like:

```bash
ai-memory status                      # server healthy, page counts unchanged
grep -L "^type:" <data_dir>/wiki/*/*/*/*.md   # no output = all pages typed
```

## Restoring the backup

Blunt and complete — returns the entire data directory to its exact
pre-migration state:

```bash
# 1. stop the server (docker compose down / systemctl stop ai-memory)
# 2. move the current data dir aside
mv <data_dir> <data_dir>.post-migration
# 3. unpack the archive as the new data dir
mkdir <data_dir>
tar -xzf ~/ai-memory-backup-okf-v0.2-<date>.tar.gz -C <data_dir>
# 4. start the OLD (1.x) binary against it
```

Surgical alternative (keeps post-migration work, reverts only the wiki
files): the `pre-okf-migration checkpoint` commit in the wiki's git
history, followed by `ai-memory reindex`.

## Downgrade guard

A 2.0-migrated data directory records the migration in the
`wiki_migrations` table. A **newer** binary opening an **older** wiki
migrates it (as above). An **older 2.0+** binary opening a **newer**
wiki refuses to start with `NewerWikiFormat` instead of silently mixing
formats. (1.x binaries predate the guard: they can open a migrated
directory and will tolerate the extra frontmatter, but new writes from
1.x will not carry the OKF keys — avoid mixing; restore the archive if
you need to stay on 1.x.)

## Sharing bundles

Post-migration, each project directory *is* an OKF v0.2 bundle. Hand a
copy to any OKF-aware tool, or export a validated tarball with a fresh
index:

```bash
ai-memory export-okf --project myproject -o myproject-bundle.tar.gz
```

Importing a foreign bundle needs no command: unpack its concept files
into a project's wiki directory and the watcher (or `ai-memory
reindex`) ingests them; anything missing from their frontmatter is
filled at write time.
