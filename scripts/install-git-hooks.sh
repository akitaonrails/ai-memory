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

repo_root=$(git rev-parse --show-toplevel)
hook="$repo_root/.git/hooks/pre-push"
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
