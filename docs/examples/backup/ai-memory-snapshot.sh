#!/usr/bin/env bash
# ai-memory-snapshot.sh
#
# Daily snapshot of an ai-memory single-user install:
#   1. rsync the mutable data dir into a git-tracked mirror checkout,
#      excluding derived state and secrets (see .gitignore in this dir).
#   2. commit + push the mirror to a remote git repository when anything
#      changed.
#   3. write a compressed tarball of the same tree to a second location
#      (a mounted external disk, a network share, or on WSL2 the Windows-side
#      filesystem) so the SQLite index can be recovered from disk even if the
#      remote repository is unreachable.
#
# Assumptions:
#   - You already ran `git init && git remote add origin <url>` inside
#     $REPO_DIR and staged the .gitignore shipped alongside this file.
#   - The user running this script has an ssh-agent or credential helper set
#     up so `git push` is non-interactive.
#   - $DATA_SRC is the ai-memory data dir (`ai-memory status` prints it) and
#     $CFG_SRC is where your provider env-file lives.
#
# Configure these four variables for your host, or export them from the
# systemd unit's Environment= lines. Sample values are commented out.
: "${REPO_DIR:=$HOME/ai-memory-backup}"           # local mirror checkout, `git init`'d and pointed at a remote
: "${DATA_SRC:=$HOME/.local/share/ai-memory}"     # ai-memory data dir
: "${CFG_SRC:=$HOME/.config/ai-memory}"           # ai-memory config dir (may hold provider env files)
: "${TARBALL_DIR:=$HOME/backups/ai-memory}"       # second-location tarball drop (external disk, network share, ...)
: "${LOG_DIR:=$HOME/logs/ai-memory-snapshot}"     # rotated per run
: "${TARBALL_RETENTION_DAYS:=30}"                 # how long to keep tarballs
: "${LOG_RETENTION_DAYS:=30}"                     # how long to keep run logs
: "${SECRET_EXCLUDES:=*.env auth.json .secrets *.pem *.key *.crt}"  # glob patterns for files never to sync

set -euo pipefail

stamp=$(date +%Y-%m-%d)
log="$LOG_DIR/ai-memory-snapshot_$(date +%Y%m%d-%H%M%S).log"
mkdir -p "$LOG_DIR" "$TARBALL_DIR"
exec >"$log" 2>&1

echo "=== $(date -Iseconds) ai-memory snapshot start ==="
echo "data_src=$DATA_SRC"
echo "cfg_src=$CFG_SRC"
echo "repo_dir=$REPO_DIR"
echo "tarball_dir=$TARBALL_DIR"

if [ ! -d "$DATA_SRC" ]; then
    echo "ERROR: $DATA_SRC does not exist. Aborting." >&2
    exit 1
fi
if [ ! -d "$REPO_DIR/.git" ]; then
    echo "ERROR: $REPO_DIR is not a git repository (run 'git init' and 'git remote add origin <url>' first)." >&2
    exit 1
fi

# Build a comma-free rsync exclude list. The .gitignore in the mirror repo
# also keeps derived state out of commits, but excluding here keeps the rsync
# working set small.
rsync_excludes=(
    --exclude=.git/
    --exclude=models/         # embedder weights, redownloadable
    --exclude=db/             # SQLite index, derived from wiki via `ai-memory reindex`
    --exclude=logs/           # rotated tracing output
    --exclude=.serve.lock     # server pidfile
)
for glob in $SECRET_EXCLUDES; do
    rsync_excludes+=(--exclude="$glob")
done

echo "--- rsync data ---"
rsync -a --delete "${rsync_excludes[@]}" "$DATA_SRC/" "$REPO_DIR/data/"

echo "--- rsync config (secrets excluded) ---"
config_excludes=()
for glob in $SECRET_EXCLUDES; do
    config_excludes+=(--exclude="$glob")
done
rsync -a --delete "${config_excludes[@]}" "$CFG_SRC/" "$REPO_DIR/config/"

cd "$REPO_DIR"
git add -A

if git diff --cached --quiet; then
    echo "no changes since last snapshot; skipping commit + push"
else
    git commit -m "auto: snapshot $stamp" | tail -3
    echo "--- push ---"
    timeout 180 git push origin "$(git rev-parse --abbrev-ref HEAD)" 2>&1 | tail -10
fi

echo "--- tarball to second location ---"
tarball="$TARBALL_DIR/ai-memory-$stamp.tar.gz"
tar_excludes=(
    --exclude=.git
    --exclude=models
    --exclude=db
    --exclude=logs
    --exclude=.serve.lock
)
for glob in $SECRET_EXCLUDES; do
    tar_excludes+=(--exclude="$glob")
done
# Use paths relative to $HOME so the archive is portable across usernames.
tar czf "$tarball" "${tar_excludes[@]}" \
    -C "$HOME" \
    "${DATA_SRC#$HOME/}" \
    "${CFG_SRC#$HOME/}"
echo "tarball: $(ls -lh "$tarball" | awk '{print $5, $9}')"

echo "--- rotate tarballs (retention: ${TARBALL_RETENTION_DAYS}d) ---"
find "$TARBALL_DIR" -maxdepth 1 -name 'ai-memory-*.tar.gz' -type f \
    -mtime "+${TARBALL_RETENTION_DAYS}" -delete -print

echo "--- rotate logs (retention: ${LOG_RETENTION_DAYS}d) ---"
find "$LOG_DIR" -maxdepth 1 -name 'ai-memory-snapshot_*.log' -type f \
    -mtime "+${LOG_RETENTION_DAYS}" -delete -print

echo "=== $(date -Iseconds) ai-memory snapshot done ==="
