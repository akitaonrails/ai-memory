#!/usr/bin/env bash
# Installs this repo's pre-push hook into .git/hooks without discarding an
# existing user hook. Run once per clone (from Git Bash on Windows):
#
#   scripts/install-git-hooks.sh
#
# The hook runs the full test tier (`cargo tf`) before a push. The everyday
# `cargo t` skips the slow/stress tier, and skipping in the inner loop is only
# safe if something catches it later. Bypass for a work-in-progress branch with
# `git push --no-verify`.

set -euo pipefail

# Prefer --git-path so worktrees (where .git is a file) and linked checkouts
# resolve to the shared common-dir hooks/, not $toplevel/.git/hooks.
hooks_dir=$(git rev-parse --git-path hooks)
hook="$hooks_dir/pre-push"
mkdir -p "$hooks_dir"
begin="# >>> ai-memory pre-push >>>"
end="# <<< ai-memory pre-push <<<"
tmp=$(mktemp "${hook}.XXXXXX")
trap 'rm -f "$tmp"' EXIT

if [[ -f "$hook" ]]; then
    awk -v begin="$begin" -v end="$end" '
        $0 == begin { skip = 1; next }
        $0 == end { skip = 0; next }
        !skip { print }
    ' "$hook" > "$tmp"
    if grep -q '[^[:space:]]' "$tmp"; then
        printf '\n' >> "$tmp"
    else
        printf '%s\n\n' '#!/usr/bin/env bash' '# Installed by scripts/install-git-hooks.sh.' > "$tmp"
    fi
else
    printf '%s\n\n' '#!/usr/bin/env bash' '# Installed by scripts/install-git-hooks.sh.' > "$tmp"
fi

cat >> "$tmp" <<'HOOK'
# >>> ai-memory pre-push >>>
# Runs the full test tier before a push. See scripts/install-git-hooks.sh.
set -euo pipefail

# Git exports GIT_DIR / GIT_WORK_TREE / … into hook environments. The test
# suite creates throwaway repos via `git` and libgit2; those calls inherit
# the hook env and then operate on *this* checkout (empty fixture commits
# have landed on the branch under push). Clear git's own local-env list
# before spawning cargo so fixtures stay inside their tempdirs.
# shellcheck disable=SC2046
unset $(git rev-parse --local-env-vars)

# macOS: stop reqwest re-reading the Keychain in every test process.
if [ "$(uname -s 2>/dev/null || true)" = "Darwin" ] && [ -z "${SSL_CERT_FILE:-}" ] && [ -f /etc/ssl/cert.pem ]; then
    export SSL_CERT_FILE=/etc/ssl/cert.pem
fi

if command -v cargo-nextest >/dev/null 2>&1; then
    echo "pre-push: cargo nextest run --workspace -P full"
    cargo nextest run --workspace -P full
else
    echo "pre-push: cargo test --workspace --all-targets (nextest not installed)"
    cargo test --workspace --all-targets
fi
# <<< ai-memory pre-push <<<
HOOK

mv "$tmp" "$hook"
trap - EXIT
chmod +x "$hook"
echo "installed $hook"
