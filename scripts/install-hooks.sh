#!/usr/bin/env bash
# Curl-based installer for ai-memory's lifecycle-hook scripts.
#
# Use when you don't want to clone the repo and don't want to use the
# docker image to extract the bundle. Downloads a checksum-verified hook
# archive from a GitHub Release, then installs only the requested agent files.
#
# Usage:
#   ai-memory-install-hooks --agent claude-code
#
# Options:
#   --agent <claude-code|codex|command-code|cursor|gemini-cli|kimi-code|kiro-cli|antigravity-cli|grok|opencode|opencode2|openclaw|omp|oh-my-pi|pi>
#                                                which agent (default: claude-code;
#                                                generated-plugin agents print hints)
#   --to <dir>                               install root (default: $HOME/.ai-memory/hooks)
#   --ref <release-tag>                      release tag to pull (default: latest)
#   --repo <owner/repo>                      release repository (default: akitaonrails/ai-memory)
#
# After installation, render the matching agent config snippet:
#   ai-memory install-hooks --agent claude-code --hooks-dir ~/.ai-memory/hooks
#
# If you only have docker:
#   docker run --rm ai-memory install-hooks --agent claude-code \
#       --hooks-dir ~/.ai-memory/hooks

set -euo pipefail

AGENT="claude-code"
TO="$HOME/.ai-memory/hooks"
REF="latest"
REPO="akitaonrails/ai-memory"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --agent)  AGENT="$2"; shift 2 ;;
        --to)     TO="$2"; shift 2 ;;
        --ref)    REF="$2"; shift 2 ;;
        --repo)   REPO="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,18p' "$0"
            exit 0 ;;
        *)
            echo "unknown flag: $1" >&2
            exit 64 ;;
    esac
done

case "$AGENT" in
    claude-code|codex|command-code|cursor|gemini-cli|kimi-code|kiro-cli|antigravity-cli|grok|opencode|opencode2|openclaw|omp|pi|oh-my-pi) ;;
    commandcode|cmdc|cmd) AGENT="command-code" ;;
    kiro) AGENT="kiro-cli" ;;
    opencode-v2|open-code2) AGENT="opencode2" ;;
    *)
        echo "unsupported agent: $AGENT (expected claude-code | codex | command-code | cursor | gemini-cli | kimi-code | kiro-cli | antigravity-cli | grok | opencode | opencode2 | openclaw | omp | pi | oh-my-pi)" >&2
        exit 64 ;;
esac

if [[ ! "$REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
    echo "invalid --repo: expected owner/repository" >&2
    exit 64
fi
if [[ ! "$REF" =~ ^[A-Za-z0-9._-]+$ ]]; then
    echo "invalid --ref: expected a release tag without path separators" >&2
    exit 64
fi

if [[ "$AGENT" == "opencode" ]]; then
    echo "OpenCode uses a generated TypeScript plugin, not shell hook scripts."
    echo "Run: ai-memory install-hooks --agent opencode --apply"
    echo "Then restart OpenCode so it loads ~/.config/opencode/plugins/ai-memory.ts."
    exit 0
fi

if [[ "$AGENT" == "opencode2" ]]; then
    echo "OpenCode 2 uses a generated TypeScript plugin, not shell hook scripts."
    echo "Run: ai-memory install-hooks --agent opencode2 --apply"
    echo "Then restart OpenCode 2 so it loads ~/.config/opencode/plugins/ai-memory-opencode2.ts."
    exit 0
fi

if [[ "$AGENT" == "openclaw" ]]; then
    echo "OpenClaw uses a generated native TypeScript plugin, not shell hook scripts."
    echo "Run: ai-memory install-hooks --agent openclaw --apply"
    echo "Then restart the OpenClaw gateway if it does not auto-restart after plugin install."
    exit 0
fi

if [[ "$AGENT" == "omp" || "$AGENT" == "oh-my-pi" ]]; then
    echo "OMP uses a generated TypeScript extension, not shell hook scripts."
    echo "Run: ai-memory install-hooks --agent omp --apply"
    echo "Then restart OMP so it loads ~/.omp/agent/extensions/ai-memory.ts."
    exit 0
fi

if [[ "$AGENT" == "pi" ]]; then
    echo "Pi uses a generated TypeScript extension, not shell hook scripts."
    echo "Run: ai-memory install-hooks --agent pi --apply"
    echo "Then restart Pi so it loads ~/.pi/agent/extensions/ai-memory.ts."
    echo "MCP tools come through the same generated bridge extension."
    exit 0
fi

SCRIPTS=(
    "session-start"
    "user-prompt-submit"
    "pre-tool-use"
    "post-tool-use"
    "pre-compact"
    "stop"
    "session-end"
)

# kimi-code also captures the subagent lifecycle (its bundle mirrors
# hooks/claude-code/, which ships them); the default list omits them.
if [[ "$AGENT" == "kimi-code" ]]; then
    SCRIPTS+=("subagent-start" "subagent-stop")
fi

# kiro-cli v2's hook vocabulary is exactly five events —
# no pre-compact/session-end equivalents exist, so fetching them would
# 404 and abort the install.
if [[ "$AGENT" == "kiro-cli" ]]; then
    SCRIPTS=(
        "session-start"
        "user-prompt-submit"
        "pre-tool-use"
        "post-tool-use"
        "stop"
    )
fi

# Command Code's stable hook API exposes exactly these four events. Additional
# lifecycle boundaries belong to its experimental Mod API and are not shipped.
if [[ "$AGENT" == "command-code" ]]; then
    SCRIPTS=(
        "session-start"
        "pre-tool-use"
        "post-tool-use"
        "stop"
    )
fi

DEST="$TO/$AGENT"
mkdir -p "$DEST"

if [[ "$REF" == "latest" ]]; then
    BASE_URL="https://github.com/$REPO/releases/latest/download"
else
    BASE_URL="https://github.com/$REPO/releases/download/$REF"
fi
ARCHIVE="ai-memory-hooks.tar.gz"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading checksum-verified ai-memory hook bundle from $BASE_URL"
curl -fsSL "$BASE_URL/$ARCHIVE" -o "$TMP/$ARCHIVE"
curl -fsSL "$BASE_URL/$ARCHIVE.sha256" -o "$TMP/$ARCHIVE.sha256"
expected_sum="$(awk 'NR == 1 && $1 ~ /^[0-9A-Fa-f]{64}$/ { print tolower($1) }' "$TMP/$ARCHIVE.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
    actual_sum="$(sha256sum "$TMP/$ARCHIVE" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
    actual_sum="$(shasum -a 256 "$TMP/$ARCHIVE" | awk '{ print $1 }')"
else
    echo "sha256sum or shasum is required to verify the hook bundle" >&2
    exit 69
fi
if [[ -z "$expected_sum" || "$actual_sum" != "$expected_sum" ]]; then
    echo "hook bundle checksum mismatch; refusing installation" >&2
    exit 1
fi
echo "Installing ai-memory hooks for $AGENT into $DEST"
for name in "${SCRIPTS[@]}"; do
    member="hooks/$AGENT/${name}.sh"
    source="$TMP/${name}.sh"
    out="$DEST/${name}.sh"
    if tar -xOf "$TMP/$ARCHIVE" "$member" > "$source" && [[ -s "$source" ]]; then
        install -m 0755 "$source" "$out"
        echo "  ✓ $name"
    else
        echo "  ✗ $name (missing from verified release bundle)" >&2
        exit 1
    fi
done

echo
echo "Done. Next steps:"
echo
echo "  1. Render the config snippet to merge into your agent's settings:"
echo "       ai-memory install-hooks --agent $AGENT --hooks-dir $TO"
echo "     (Or via docker if you don't have the binary locally:"
echo "       docker run --rm $REPO:latest install-hooks --agent $AGENT --hooks-dir $TO)"
echo
echo "  2. If your server uses bearer-token auth, pass --auth-token <token>"
echo "     to the install-hooks command so the snippet wires it in."
