# Backups and remote git mirror

> This document covers backup patterns beyond the on-box `ai-memory backup
> --to <tarball>` snapshot documented in
> [`docs/lifecycle-ops.md#backup`](lifecycle-ops.md#backup) and the "point-in-time
> consistency" `scp` idiom in [`docs/deploy.md#backups`](deploy.md#backups). If
> you already have a working tarball routine, treat this as an additional
> layer.

Ai-memory's wiki directory is a real git repository. Every consolidation pass
writes a checkpoint through libgit2 (see cross-cutting invariant #10 in
[`AGENTS.md`](../AGENTS.md) and the wiki design notes in
[`docs/ARCHITECTURE.md`](ARCHITECTURE.md)). That means half of a remote-backup
setup is already done for you — the wiki has its own commit history and can be
pushed to any git remote you own. The rest of this doc is about wiring that
up, what else to include, what to exclude, and how to schedule it.

## What the data dir contains, and what to back up

`ai-memory status` prints the resolved data dir; per
[`docs/deploy.md#backups`](deploy.md#backups) it looks like this:

```
data/
├── wiki/    # markdown source of truth (already a git repository)
├── raw/    # immutable session-log ledger
├── db/     # memory.sqlite (FTS5, entities, embeddings) — DERIVED
├── logs/   # daily rolling tracing
└── models/ # local embedder weights (redownloadable)
```

Split by durability:

| Directory | Back up? | Why |
|---|---|---|
| `wiki/` | **Yes, always.** | Markdown source of truth. Irreplaceable. Already a git repository. |
| `raw/` | Yes if you want the full ledger; no if you want a small mirror. | Immutable observation log. Useful for rebuilding session history; not required to keep the wiki working. |
| `db/` | **No.** | SQLite index derived from `wiki/`. Rebuildable via `ai-memory reindex`; see [`lifecycle-ops.md#reindex`](lifecycle-ops.md#reindex). Committing a WAL-mode SQLite database to git also wastes space on every write. |
| `logs/` | No. | Tracing output; nothing durable. |
| `models/` | No. | Local embedder weights, sha256-pinned, redownloaded on first boot per [`local-embeddings.md`](local-embeddings.md). |
| `.serve.lock` | No. | Server pidfile. |
| `config.toml` and env files under `~/.config/ai-memory/` | Yes, minus secrets. | Provider config is useful for reproducing an install; API-key env files (`*.env`), OAuth tokens (`auth.json`), and private CA bundles must never land in a mirror. |

If you also need the SQLite index for near-instant recovery (rather than
running `reindex` after a restore), pair the git mirror with a periodic
`ai-memory backup --to <tarball>` and stash the tarball on a second location
(external disk, network share, or on WSL2 the Windows-side filesystem via
`/mnt/c/`).

## Security: what ends up in your wiki

Read this before configuring a remote — decide what is acceptable to have
off your machine, then pick the shape of backup that matches.

The wiki captures **everything the coding agent sees during a session**
that ai-memory's sanitizer classifies as observations to store: user
prompts, tool inputs and outputs, page bodies written by consolidation.
The sanitizer strips obvious credentials (see `SECURITY.md`), but it
cannot prove that every prompt or tool response was safe to store. In
practice this means: if any user has ever pasted an API key, a password,
a private client name, a customer record, or a file path they consider
sensitive into a captured session, it is on disk in the wiki tree, and
whatever you push will carry it.

For any remote target — even a private repository on a service you
control — assume an operator or administrator on the receiving side
could read the contents. Employer-managed cloud repositories
(GitHub Enterprise, GitLab self-hosted, Bitbucket Data Center) may be
routinely audited by administrators, and a repository private to you on
a shared account is still readable by whoever administers the account.

**Suggested exclusions**, in addition to the derived-state and
secret-file rules already covered above. The mirror repository's
`.gitignore` should carry at least:

```
# Provider credentials and OAuth material — never push
config/*.env
config/*.key
config/*.pem
config/auth.json
config/.secrets

# Derived index and downloadable state
data/models/
data/db/
data/logs/
data/.serve.lock
```

The example `.gitignore` under [`docs/examples/backup/`](examples/backup/README.md)
ships this list already; expand it for anything install-specific
(host-side certificates, private CA bundles, cloud credential files,
site-specific env files).

**Alternatives when the wiki content is too sensitive to store as text
on a remote** — pick whichever fits the threat model:

- **Encrypted archive to object storage.** `restic` or `borg` snapshots
  of the data dir to a private object store (S3, Backblaze B2, Wasabi,
  or a self-hosted MinIO) with a passphrase-derived encryption key. The
  content leaves the host encrypted; the object-store operator sees
  ciphertext.
- **Selective encryption inside the mirror repo.** `git-crypt` on paths
  known to carry sensitive material (for example `data/wiki/**/*.md`)
  keeps the git-remote workflow but encrypts blobs before push. Set up
  keys before the first commit; retro-encryption of history is manual.
- **`age`-encrypted tarball to a second location.** Simpler than
  `restic`/`borg`, no state file: pipe `tar czf - <data-dir>` through
  `age -R <recipients>` and drop the ciphertext onto an external disk
  or a network share.
- **Local-only backup, no remote.** Rotate encrypted tarballs onto an
  external USB drive that lives off-site; give up remote-git
  convenience for zero third-party exposure.

If none of the wiki content is more sensitive than what already lives
in your project's source repository, a private mirror repo is fine —
this section is for the case where it is.

## Backing up the wiki to a remote git repository

The wiki is already a git repository. If your only goal is off-site *wiki*
storage, adding a remote and pushing is enough:

```bash
data_dir=$(ai-memory status --json | jq -r .data_dir)
cd "$data_dir/wiki"
git remote add origin git@github.com:<you>/ai-memory-wiki.git
git push -u origin main
```

Two caveats:

- The wiki repo is written to by the server through libgit2; do not run
  interactive git operations (rebases, merges, force-pushes) against it while
  the server is running. Read-only commands (`git log`, `git show`, `git
  push`) are safe.
- The wiki carries page bodies verbatim, including any content the model
  synthesized. Treat the remote the same way you treat any project source:
  private repository, restrictive access controls.

## Backing up the whole data dir (mirror + push)

For a full-install backup — wiki, raw ledger, sanitized config — the
supported pattern is a scheduled rsync into a mirror git repository, followed
by a git push. This avoids running git operations against the live wiki
repository (which the server writes to) while still giving you a single
remote with the full recoverable state.

A worked example lives at
[`docs/examples/backup/`](examples/backup/README.md) and consists of:

- `ai-memory-snapshot.sh` — an idempotent rsync + `git commit && git push` +
  tarball script driven by six environment variables.
- `ai-memory-backup.service` — a `systemd --user` oneshot unit that runs it.
- `ai-memory-backup.timer` — a daily user timer with a randomised delay and
  `Persistent=true` so a missed run after a reboot still fires.
- `.gitignore` — an exclusion list to drop into the root of the mirror
  repository so derived state and secrets never land in commits.

The high-level flow, one invocation per day (adjust the timer for a different
cadence):

1. `rsync -a --delete` the data dir into the mirror checkout, applying an
   exclude list for `models/`, `db/`, `logs/`, `.serve.lock`, and any glob
   listed in `SECRET_EXCLUDES` (`*.env`, `auth.json`, `.secrets`, `*.pem`, `*.key`, `*.crt` by default).
2. `rsync -a --delete` the config dir under the same exclude rules so
   `config.toml` is captured but env files, OAuth tokens, and private CA
   bundles are not.
3. `git add -A && git commit -m "auto: snapshot $(date +%F)"` if anything
   changed; skip if there is no delta.
4. `git push origin main` with a timeout so a hung network does not block the
   next run.
5. `tar czf $TARBALL_DIR/ai-memory-$(date +%F).tar.gz` a copy of the same
   tree, so the DB and models can be recovered without a `reindex` after a
   catastrophic disk loss.
6. Rotate old tarballs and old script logs on a retention window (30 days by
   default).

See the example directory's [README](examples/backup/README.md) for the
install-and-enable steps and non-systemd equivalents (macOS launchd, Windows
Task Scheduler via WSL, Docker sidecar).

## Restoring from a mirror repository

The mirror repository is a copy of the data dir minus derived state.
Recovering the full ai-memory install from it is two steps:

1. Clone the mirror onto the target host, then rsync the `data/` and
   `config/` subtrees into place under the ai-memory data dir and config dir
   respectively.
2. Rebuild the SQLite index from the recovered markdown:
   [`lifecycle-ops.md#reindex`](lifecycle-ops.md#reindex) walks through this.
   Sessions, observations, handoffs, users/tokens, audit rows, and
   embeddings live only in the DB and are not rebuilt by `reindex` — for
   those, restore from the paired tarball via `ai-memory restore --from
   <tarball> --data-dir <path> --force`.

## Security posture

Remote sync is your own channel, not ai-memory's — this is called out
explicitly in [`SECURITY.md`](../SECURITY.md) ("Remote sync security" under
out-of-scope for v1). What that means in practice:

- **Private repository, always.** ai-memory captures prompts, tool calls, and
  synthesized page bodies verbatim — the same content you would treat as
  project source.
- **Least-privilege push credential.** Prefer an SSH deploy key or a
  fine-grained personal-access token scoped to the mirror repository only.
  Rotate on the same cadence you rotate other long-lived push credentials.
- **Explicit secret exclusion.** The example script rejects any file matching
  `SECRET_EXCLUDES` (`*.env`, `auth.json`, `.secrets`, `*.pem`, `*.key`, `*.crt` by default), and the
  suggested mirror-repo `.gitignore` matches the same set. Add anything else
  install-specific — private CA bundles, host-side certificates, cloud
  credential files — to both lists.
- **Encryption at rest is out of scope for ai-memory.** If your threat model
  requires it, either put the mirror on an encrypted remote (`git-crypt`,
  GitLab's group-level encryption, a self-hosted Gitea over TLS with disk
  encryption) or archive the tarballs with `gpg --encrypt` before dropping
  them at `TARBALL_DIR`.

## Related documents

- [`docs/lifecycle-ops.md`](lifecycle-ops.md) — `ai-memory backup`, `restore`,
  `restore-page`, `reset`, `reindex` reference.
- [`docs/deploy.md#backups`](deploy.md#backups) — the on-box tarball routine
  and the `scp`-to-laptop idiom for Docker deployments.
- [`docs/airgapped-install.md`](airgapped-install.md) — offline installs,
  including the callout that remote git sync is user-owned plumbing.
- [`SECURITY.md`](../SECURITY.md) — the "Remote sync security" out-of-scope
  note this doc expands into a walkthrough.
- [`docs/local-embeddings.md`](local-embeddings.md) — how `models/` is
  fetched and pinned (why it does not need to be in the mirror).
