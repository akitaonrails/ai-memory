#!/usr/bin/env bash
# Installs this repo's pre-push hook without moving existing user commands.
# Run once per clone (from Git Bash on Windows):
#
#   scripts/install-git-hooks.sh
#
# The hook runs the full test tier (`cargo tf`) before a push. The everyday
# `cargo t` skips the slow/stress tier, and skipping in the inner loop is only
# safe if something catches it later. Bypass for a work-in-progress branch with
# `git push --no-verify`.

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"
if git config --get core.hooksPath >/dev/null; then
    echo 'core.hooksPath is set; this installer only manages the shared repository hook directory' >&2
    exit 1
else
    # `$?` is still the condition's status here: 1 means the key is unset, and
    # anything else is a Git failure the installer must not guess past.
    config_status=$?
    if [[ "$config_status" -ne 1 ]]; then
        exit "$config_status"
    fi
fi
hook=$(git rev-parse --git-path hooks/pre-push)
begin="# >>> ai-memory pre-push >>>"
end="# <<< ai-memory pre-push <<<"

managed_block=$(cat <<'HOOK'
# >>> ai-memory pre-push >>>
# Runs the full test tier before a push. See scripts/install-git-hooks.sh.

# Git's hook environment would redirect fixture commands into this checkout.
# Isolate Cargo and its children, and keep this block's shell options and
# exports away from user hook code around it.
(
    set -euo pipefail

    # macOS: stop reqwest re-reading the Keychain in every test process.
    if [ "$(uname -s 2>/dev/null || true)" = "Darwin" ] && [ -z "${SSL_CERT_FILE:-}" ] && [ -f /etc/ssl/cert.pem ]; then
        export SSL_CERT_FILE=/etc/ssl/cert.pem
    fi

    # A plain assignment, so `set -e` still aborts if `git rev-parse` fails.
    git_local_env_vars=$(git rev-parse --local-env-vars)
    # A surrounding user hook may have narrowed IFS; the list below is split on
    # newlines, so fall back to the default before splitting it. Unset rather
    # than assigned: Bash 3.2 expands ANSI-C quoting inside this heredoc.
    unset IFS
    # Unquoted on purpose: the output is one variable name per line.
    for git_local_env_var in $git_local_env_vars; do
        unset "$git_local_env_var"
    done
    # Fixture repositories must not inherit machine settings such as signing.
    # Git for Windows maps /dev/null to nul.
    export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null

    if command -v cargo-nextest >/dev/null 2>&1; then
        echo "pre-push: cargo nextest run --workspace -P full"
        cargo nextest run --workspace -P full
    else
        echo "pre-push: cargo test --workspace --all-targets (nextest not installed)"
        cargo test --workspace --all-targets
    fi
)
# Bash ignores `set -e` inside a subshell tested by `||`, `&&`, `!` or `if`, so
# the subshell stays a plain command and its status is propagated here. A user
# hook without `set -e` would otherwise run on and let the push through.
ai_memory_pre_push_status=$?
if [ "$ai_memory_pre_push_status" -ne 0 ]; then
    exit "$ai_memory_pre_push_status"
fi
unset ai_memory_pre_push_status
# <<< ai-memory pre-push <<<
HOOK
)

has_content=0
if [[ -f "$hook" ]]; then
    if grep -q '[^[:space:]]' "$hook"; then
        has_content=1
    else
        # As above, `$?` is grep's status: 1 is a blank hook, 2 a read error.
        read_status=$?
        if [[ "$read_status" -ne 1 ]]; then
            exit "$read_status"
        fi
    fi
fi

mkdir -p "${hook%/*}"
tmp=$(mktemp "${hook}.XXXXXX")
if [[ "$has_content" -eq 1 ]]; then
    # ENVIRON preserves backslashes; BINMODE prevents Windows CRLF translation.
    if ! AI_MEMORY_PRE_PUSH_BLOCK="$managed_block" awk -v BINMODE=3 -v begin="$begin" -v end="$end" '
        {
            marker = $0
            sub(/\r$/, "", marker)
            if (marker == begin) {
                if (inside || seen) { invalid = 1; exit 1 }
                inside = seen = 1
                print ENVIRON["AI_MEMORY_PRE_PUSH_BLOCK"]
                next
            }
            if (marker == end) {
                if (!inside) { invalid = 1; exit 1 }
                inside = 0
                next
            }
            if (!inside) print
        }
        END {
            if (invalid || inside) exit 1
            if (!seen) {
                print ""
                print ENVIRON["AI_MEMORY_PRE_PUSH_BLOCK"]
            }
        }
    ' "$hook" > "$tmp"; then
        printf 'invalid managed markers in %s; original unchanged, temporary file retained at %s\n' "$hook" "$tmp" >&2
        exit 1
    fi
else
    printf '%s\n\n' '#!/usr/bin/env bash' '# Installed by scripts/install-git-hooks.sh.' > "$tmp"
    printf '%s\n' "$managed_block" >> "$tmp"
fi

mv "$tmp" "$hook"
chmod +x "$hook"
echo "installed $hook"
