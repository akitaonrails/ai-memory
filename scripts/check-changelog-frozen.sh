#!/usr/bin/env bash
# Fail if a change adds or edits entries inside an already-released
# CHANGELOG section.
#
# `bin/release` renames `## [Unreleased]` to `## [X.Y.Z]`. A branch opened
# before that release still carries a CHANGELOG diff anchored at the old
# `[Unreleased]` line numbers, and git merges it into whatever section now
# occupies them — no conflict, no warning. The entry then claims a change
# shipped in a release that did not contain it, and the release that *did*
# ship it lists nothing.
#
# Three times in one week: #491 into [1.32.0], #502 into [1.32.2], #517 into
# [1.33.0]. Each found by hand, twice only because an unrelated merge
# conflict forced someone to look at the file.
#
# Usage: check-changelog-frozen.sh [base-ref]   (default: origin/main)
#
# Compares the merge base of <base-ref> and HEAD against HEAD, and rejects
# any changed CHANGELOG line that sits below the `## [Unreleased]` heading.
# Deliberate historical corrections on the default branch are unaffected:
# this asks what a branch changes relative to where it forked, not what the
# file looks like versus a tag.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

BASE_REF="${1:-origin/main}"
git rev-parse --verify "$BASE_REF" >/dev/null 2>&1 || {
    echo "check-changelog-frozen: base ref '$BASE_REF' not found; skipping."
    exit 0
}

MERGE_BASE="$(git merge-base "$BASE_REF" HEAD)"
if [[ "$MERGE_BASE" == "$(git rev-parse HEAD)" ]]; then
    echo "check-changelog-frozen: HEAD is an ancestor of $BASE_REF; nothing to check."
    exit 0
fi

# First line number of the first released section in the *new* file.
FROZEN_FROM="$(awk '/^## \[[0-9]/{print NR; exit}' CHANGELOG.md)"
if [[ -z "$FROZEN_FROM" ]]; then
    echo "check-changelog-frozen: no released section yet."
    exit 0
fi

# Every new-file line number this diff touches, via the unified hunk headers.
TOUCHED="$(git diff --unified=0 "$MERGE_BASE" HEAD -- CHANGELOG.md \
    | awk '/^@@/{ split($3, a, ","); start = a[1] + 0; if (start < 0) start = -start;
                  count = (a[2] == "" ? 1 : a[2] + 0);
                  for (i = 0; i < count; i++) print start + i }')"

BAD=""
for ln in $TOUCHED; do
    if (( ln >= FROZEN_FROM )); then
        BAD+="  line $ln: $(sed -n "${ln}p" CHANGELOG.md)"$'\n'
    fi
done

if [[ -z "$BAD" ]]; then
    echo "CHANGELOG: no released section was modified."
    exit 0
fi

echo "error: this change modifies an already-released CHANGELOG section." >&2
echo >&2
printf '%s' "$BAD" >&2
cat >&2 <<'EOF'

Released sections are frozen. An entry for unreleased work belongs under
`## [Unreleased]`.

This usually means the branch was opened before a release was cut, and git
merged the entry into the section that now occupies those lines. Move it up
to `[Unreleased]`; the content does not change, only the section.
EOF
exit 1
