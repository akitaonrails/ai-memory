# Remote git backup example

Sample scripts + unit files that push an ai-memory install to a remote git
repository on a daily cadence, keeping a tarball on a second location for the
SQLite index and models cache that the mirror repo deliberately excludes.

See [`docs/backup.md`](../../backup.md) for the walkthrough. The pieces in this
directory are:

- `ai-memory-snapshot.sh` — the rsync + git push + tarball loop. Configurable
  through six environment variables (`REPO_DIR`, `DATA_SRC`, `CFG_SRC`,
  `TARBALL_DIR`, `LOG_DIR`, `SECRET_EXCLUDES`) with sensible defaults for a
  single-user Linux/WSL/macOS install.
- `ai-memory-backup.service` — a `systemd --user` oneshot that runs the
  script. `%h` expands to the user's home directory.
- `ai-memory-backup.timer` — a daily user timer with a small randomised delay
  and `Persistent=true` so the missed run after a reboot still fires.
- `.gitignore` — the exclusion list to drop into the root of the mirror
  repository. Belt-and-braces to the rsync/tar exclude lists in the script
  itself.

## Quickstart

```bash
# 1. Create a private repository on your git host (e.g. github.com), then:
mkdir -p ~/ai-memory-backup
cd ~/ai-memory-backup
git init
git remote add origin git@github.com:<you>/ai-memory-backup.git
cp .../docs/examples/backup/.gitignore .
git add .gitignore
git commit -m "initial: ignore secrets, DB, models, logs"
git branch -M main
# push once so `snapshot.sh` has a remote to push to
git push -u origin main

# 2. Drop the snapshot script somewhere on $PATH.
install -Dm755 \
    .../docs/examples/backup/ai-memory-snapshot.sh \
    ~/.local/bin/ai-memory-snapshot.sh

# 3. Install the unit files.
install -Dm644 \
    .../docs/examples/backup/ai-memory-backup.service \
    ~/.config/systemd/user/ai-memory-backup.service
install -Dm644 \
    .../docs/examples/backup/ai-memory-backup.timer \
    ~/.config/systemd/user/ai-memory-backup.timer

# 4. Enable + start.
systemctl --user daemon-reload
systemctl --user enable --now ai-memory-backup.timer

# 5. Trigger one snapshot to confirm the plumbing works.
systemctl --user start ai-memory-backup.service
journalctl --user -u ai-memory-backup.service --since '5 minutes ago'
```

## Non-systemd equivalents

The script is a plain bash program; anything that runs a shell command on a
schedule can drive it:

- **macOS launchd:** wrap `ai-memory-snapshot.sh` in a `.plist` under
  `~/Library/LaunchAgents/` with a `StartCalendarInterval` block.
- **Windows Task Scheduler:** run the script inside WSL via `wsl -e bash -lc
  'ai-memory-snapshot.sh'` on a daily trigger. On native Windows use the
  PowerShell equivalent of the rsync + `git add/commit/push` + tar sequence.
- **Docker sidecar:** a small cron container mounted at the ai-memory data
  volume, running the same script.

## Security note

The remote is your own channel, not ai-memory's. Follow the existing
[`SECURITY.md`](../../../SECURITY.md) guidance ("Remote sync security"): use a
private repository, a fine-grained token scoped to that one repo (or an SSH
deploy key), and rotate credentials the same way you rotate any other
long-lived push credential. The script never commits files matched by
`SECRET_EXCLUDES` (default: `*.env`, `auth.json`, `.secrets`, `*.pem`, `*.key`, `*.crt`); the mirror
repo's `.gitignore` catches anything the rsync exclude missed.
