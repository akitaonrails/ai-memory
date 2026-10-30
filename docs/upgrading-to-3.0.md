# Upgrading to 3.0

3.0 is a major release because it removes the 1.x compatibility-token
commands. Most 2.x installs upgrade with no action: the server applies its
schema migrations on the first start, and existing tokens, pages, sessions
and handoffs carry over. Read this page if you script user management, run a
multi-user server, or want to know which defaults changed.

For the full list of changes, see the 3.0.0 section of
[`CHANGELOG.md`](../CHANGELOG.md).

## Before you upgrade

1. Search your scripts, CI jobs and runbooks for `ai-memory user add`,
   `user expire`, `user revive`, `user rotate-token`, and for HTTP calls to
   `POST /admin/users` or `/admin/users/<name>/expire`, `/revive` and
   `/rotate-token`. These are gone in 3.0. The
   [replacement table](#the-1x-token-commands-are-gone) below lists what to
   use instead.
2. Take a backup while the 2.x server is running:

   ```bash
   ai-memory backup --to ~/ai-memory-pre-3.0.tar.gz
   ```

   The backup uses SQLite's online backup, so the server can keep running.
   It is your only way back to 2.x (see [Rolling back](#rolling-back)).
3. Note the 2.x version you run (`ai-memory --version`) in case you need to
   reinstall it.
4. If you use the `ai-memory-wikisync` or `ai-memory-importer` companions,
   upgrade the server first. Their 3.0 versions refuse to write to a server
   older than 3.0. The first `ai-memory-wikisync` run after the upgrade
   rewrites unedited files from an earlier export so they carry the new
   frontmatter.
5. The Claude Code and Codex marketplace plugins need ai-memory 3.0 or later.
   Upgrade the binary before you install them.

## Upgrade steps

1. Upgrade the binary or image the way you installed it:
   - Native release binary or Docker wrapper: `ai-memory upgrade`.
   - Homebrew, AUR or another package manager: that package manager's
     upgrade command.
   - Docker Compose deployment: `docker compose pull && docker compose up -d`
     in the deploy directory.
   - A server on another host: upgrade it there as well. `ai-memory upgrade`
     on a client machine only refreshes the local client and hooks.
2. Restart the server if the upgrade did not. On the first start it applies
   migrations V77 to V80 before it accepts traffic. V78, V79 and V80 rebuild
   the `sessions` and `users` tables (V78 and V80 `sessions`, V79 `users`), so
   the first start on a large store can take longer than usual.
3. Check the result:

   ```bash
   ai-memory doctor
   ai-memory status
   ```

4. If anyone on the server used the 1.x token commands, list the tokens they
   issued:

   ```bash
   ai-memory api-key list
   ```

## What changes

### The 1.x token commands are gone

`ai-memory user add`, `user expire`, `user revive` and `user rotate-token`
were deprecated in 2.0 and are removed, together with their admin endpoints.
`POST /admin/users` now answers 405, and the three per-user routes answer 404.
Every other `user` subcommand (`add-human`, `list`, `reset-password`,
`disable`, `enable`, `patch`, `grant`, `revoke`, `grants`) is unchanged.

Tokens those commands issued keep working. Since 2.0 each one has been stored
as an API key with the label `legacy-user-token`, and authentication has read
only API keys. After the upgrade they authenticate exactly as before, as the
same non-root user. A token that was expired before the upgrade stays
refused.

Manage them with the `api-key` commands. Each legacy key's id is the id of the
user it belongs to, which `ai-memory user list --json` prints.

| 2.x command | 3.0 replacement |
|---|---|
| `user add --username alice` | `user add-human --username alice` for a person (prints a temporary password), then `api-key add --username alice --label <name>` for a machine credential |
| `user expire alice` | `api-key revoke <id>` |
| `user rotate-token alice` | `api-key rotate <id>`, which prints a new secret and retires the old one |
| `user revive alice` | `api-key rotate <id>`: it reactivates the key with a new secret. The old secret cannot be restored. |

Over HTTP, create people with `POST /admin/human-users` and keys with
`POST /admin/api-credentials`.

Migration V79 drops the `users.token_hash` and `users.token_expired_at`
columns, which nothing used once the commands were gone. User records from
`GET /admin/users` and `ai-memory user list --json` no longer carry
`token_expired_at`, and `ai-memory user list` no longer shows an `expired`
status. Check `api-key list` for a key's state instead.

### New installs require a token; existing installs do not change

`ai-memory init` on a fresh data directory now creates a root bearer token at
`<data_dir>/auth-tokens/default` and turns auth on. An existing install keeps
running without auth after the upgrade. To turn it on, run
`ai-memory auth enable` and restart the server. It reuses or creates the token
file, gives it to the installed hooks, and re-registers the MCP clients that
point at this server. `ai-memory status` and `ai-memory doctor` now print the
server's auth posture.

A browser that signs in to the web UI with HTTP Basic no longer gets an
`ai_memory_auth` cookie. A cookie a 2.x server set keeps working until human
auth is enabled.

### `memory_query` searches observations by default

The observation stream is now on by default: project and `scopes` searches
also match raw session observations and rank the session page that holds the
answer. To get the 2.x ranking back, set this in `config.toml`:

```toml
[retrieval]
observation_stream = false
```

or set `AI_MEMORY_RETRIEVAL__OBSERVATION_STREAM=false` in the server's
environment.

### Scoped `/api/v1/search` uses the `memory_query` ranker

A `/api/v1/search` call with one `workspace` and `project`, or with `scopes`,
now ranks with the same hybrid ranker as `memory_query`, and `rank` is the
fused rank. If a client depended on the FTS5-only order, expect different
results. Search without a scope is unchanged.

### Re-running `backfill --force` over old imports

Backfill and recovery replay now use reserved `recovery_*` idempotency keys
(migration V77). Running `backfill --force` again over sessions that
ai-memory 2.6.0 or older imported can store some of those observations a
second time. Avoid forcing a re-import of sessions you already imported.

### 2.x projects are adopted from any checkout

A 2.x project is named after the folder it was captured from and carries no
repository identity. The first 3.x capture that brings the repository's
identity claims that project in place and, with the default `identity_style`,
renames it after the repository path (`github.com/acme/api` becomes
`acme-api`). Its UUID, pages, sessions and grants stay attached. A capture from
a linked worktree (`git worktree add`) claims the project named after its main
checkout, so it does not matter which checkout captures first. When a
worktree also had a 2.x project under its own folder name, the main
checkout's project is the one claimed and the worktree's is left as it was;
fold it in with `ai-memory merge-project --from <worktree-project> --into
<project>`, which prints a dry run until you add `--confirm`.

The server finds the main checkout by reading the worktree's git metadata, so
it has to be able to read your checkouts. A server that cannot, such as one in
a container without the host paths mounted, cannot do it. There, send the first
capture after the upgrade from the main checkout, or set
`project_strategy = "repo-root"` in a marker so the hooks name the main
checkout's project on the host (see
[Git worktrees](marker-file.md#git-worktrees--repo-root-identity)).

### Library change

The `ai-memory-llm` crate no longer exports `OPENCODE_ZEN_BASE_URL`. Use
`OPENCODE_GO_BASE_URL`, which holds the same URL. This matters only if you
build against the crate; the `opencode-zen` provider name still works.

## Rolling back

Migrations are forward-only. Once 3.0 has migrated a data directory, a 2.x
binary refuses to open it and reports that the memory database schema is
newer than that build. To go back to 2.x, restore the backup you took before
upgrading:

1. Stop the 3.0 server.
2. Reinstall the 2.x version you noted, for example
   `ai-memory upgrade --version v2.6.6` for a native release binary, or
   `AI_MEMORY_IMAGE=docker.io/akitaonrails/ai-memory:2.6.6` (or the same image
   tag in your Compose file) for Docker.
3. With the 2.x binary, restore the backup:

   ```bash
   ai-memory restore --from ~/ai-memory-pre-3.0.tar.gz --data-dir <data dir> --force
   ```

   `restore` applies the running binary's migrations to the snapshot, so
   running it with the 3.0 binary would migrate the backup forward again.
4. Start the 2.x server.

Everything written after the upgrade is lost, including API keys issued or
rotated under 3.0. A client that switched to a key rotated under 3.0 has to go
back to the token it used before, or get a new one issued under 2.x.
