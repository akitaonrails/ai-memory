# ai-memory hook helper — find marker file + parse minimal TOML.
# Sourced by per-agent lifecycle hook scripts. POSIX shell only —
# no bash-isms, no non-standard deps (no jq, no toml crate). Keep changes
# byte-trivial because every supported agent (claude-code, codex,
# cursor, gemini-cli, kimi-code, antigravity-cli, opencode, omp) sources
# this same file.

# Walk up from "$1" toward $HOME (or /) looking for `.ai-memory.toml`.
# Prints the absolute path of the first marker found, or nothing.
# Stops at $HOME to avoid leaking declarations from a shared system
# user's home into another user's session on multi-user boxes.
ai_memory_find_marker() {
    dir="$1"
    [ -z "$dir" ] && return 0
    while [ -n "$dir" ] && [ "$dir" != "/" ]; do
        if [ -f "$dir/.ai-memory.toml" ]; then
            printf '%s\n' "$dir/.ai-memory.toml"
            return 0
        fi
        if [ -n "${HOME:-}" ] && [ "$dir" = "$HOME" ]; then
            return 0
        fi
        parent=$(dirname "$dir")
        [ "$parent" = "$dir" ] && return 0
        dir="$parent"
    done
}

# Parse `key = "value"` at the TOML root (no nesting, no arrays, no
# tables). Returns the first match or nothing. Ignores comments and
# blank lines by construction (the regex only matches the `key = "..."`
# shape).
ai_memory_parse_toml_key() {
    file="$1"; key="$2"
    [ -f "$file" ] || return 0
    sed -n -E "s/^[[:space:]]*${key}[[:space:]]*=[[:space:]]*\"([^\"]*)\".*/\1/p" \
        "$file" | head -n 1
}

# Like ai_memory_parse_toml_key but also accepts a BARE value
# (`key = true` / `key = 6000`), so section-style flags such as
# `[briefing] inject_on_session_start = true` work quoted or not.
# Parity with `parse_toml_flag` in hook_capture.rs: line-based (section
# headers are ignored), first match wins, trailing `# comment` stripped.
ai_memory_parse_toml_flag() {
    file="$1"; key="$2"
    [ -f "$file" ] || return 0
    sed -n -E "s/^[[:space:]]*${key}[[:space:]]*=[[:space:]]*\"?([^\"#]*)\"?.*/\1/p" \
        "$file" | head -n 1 | sed 's/[[:space:]]*$//'
}

# Extract the first cwd-like path from a JSON payload on stdin or in $1.
# Returns the value or nothing. This is intentionally a tiny shell fallback,
# not a JSON parser; taking the first match preserves the top-level cwd when
# tool payloads contain nested `cwd` fields later in the object. Antigravity
# CLI sends `workspacePaths: ["/repo", ...]` instead of `cwd`.
# Undo the JSON string escapes that can appear in a path value: \\ -> \
# and \/ -> /. Windows payloads carry cwd as "C:\\dev\\proj"; without this
# the doubled backslashes leak into the query string (#188).
ai_memory_json_unescape_path() {
    printf '%s' "$1" | sed 's/\\\\/\\/g; s/\\\//\//g'
}

ai_memory_extract_cwd() {
    payload="${1:-$(cat)}"
    rest=${payload#*\"cwd\"}
    if [ "$rest" != "$payload" ]; then
        raw=$(printf '%s' "$rest" \
            | sed -n -E 's/^[[:space:]]*:[[:space:]]*"([^"]*)".*/\1/p' \
            | head -n 1)
        ai_memory_json_unescape_path "$raw"
        return 0
    fi
    rest=${payload#*\"workspacePaths\"}
    [ "$rest" = "$payload" ] && return 0
    raw=$(printf '%s' "$rest" \
        | sed -n -E 's/^[[:space:]]*:[[:space:]]*\[[[:space:]]*"([^"]*)".*/\1/p' \
        | head -n 1)
    ai_memory_json_unescape_path "$raw"
}

# Extract a harness-native session id from the common hook payload spellings.
# Like the cwd fallback above this intentionally handles top-level JSON strings
# only; native `ai-memory hook` uses a real JSON parser.
ai_memory_extract_session_id() {
    payload="${1:-$(cat)}"
    for key in session_id sessionId sessionID session conversationId; do
        rest=${payload#*\"$key\"}
        if [ "$rest" != "$payload" ]; then
            printf '%s' "$rest" \
                | sed -n -E 's/^[[:space:]]*:[[:space:]]*"([^"]*)".*/\1/p' \
                | head -n 1
            return 0
        fi
    done
}

ai_memory_managed_qs() {
    [ -n "${AI_MEMORY_RUN_ID:-}" ] || return 0
    printf '&managed_run=%s' "$(ai_memory_url_encode "$AI_MEMORY_RUN_ID")"
}

# Resolve cwd for agents whose native hook payload omits it. Payload wins,
# then Devin's project env var, then the hook process cwd.
ai_memory_resolve_cwd() {
    payload="${1:-$(cat)}"
    cwd=$(ai_memory_extract_cwd "$payload")
    if [ -n "$cwd" ]; then
        printf '%s' "$cwd"
        return 0
    fi
    if [ -n "${DEVIN_PROJECT_DIR:-}" ]; then
        printf '%s' "$DEVIN_PROJECT_DIR"
        return 0
    fi
    pwd 2>/dev/null || true
}

# URL-encode the minimal set of characters that have meaning in a query
# string. Sufficient for the schema's value regex (`^[a-z0-9][a-z0-9._-]*$`)
# plus a defensive pass for anything a hand-edited marker might contain.
# Percent-encode everything outside the RFC 3986 unreserved set
# (A-Z a-z 0-9 - _ . ~), byte-wise under LC_ALL=C so multibyte UTF-8 is
# encoded per byte. Allow-list on purpose: the old deny-list missed
# backslash, so a Windows cwd went into the query string raw and the
# request never reached the server (#188). Parity with the native
# helper's url_encode in hook_capture.rs.
ai_memory_url_encode() {
    LC_ALL=C
    s="$1"
    out=""
    while [ -n "$s" ]; do
        rest="${s#?}"
        c="${s%"$rest"}"
        s="$rest"
        case $c in
            [A-Za-z0-9._~-]) out="$out$c" ;;
            *) out="$out$(printf '%%%02X' "'$c")" ;;
        esac
    done
    printf '%s' "$out"
}

# Resolve the basename of the MAIN git repository root for "$1" (a cwd),
# following the worktree commondir pointer so every linked worktree of a
# repo collapses to one stable name. Mirrors the server's
# `discover_main_repo_root` (libgit2) but runs host-side, where the
# checkout is always visible — the server cannot do this when it runs in a
# container that has no access to the host filesystem (its own discovery
# fails and falls back to basename(cwd), so out-of-tree worktrees each
# became their own project). Prints the name, or nothing when cwd is not
# inside a git work tree (caller keeps its basename(cwd) fallback).
ai_memory_repo_root_project() {
    cwd="$1"
    [ -z "$cwd" ] && return 0
    command -v git >/dev/null 2>&1 || return 0
    # Only touch git when cwd is genuinely inside a working tree. Outside any
    # repo, or inside a bare repo, `--is-inside-work-tree` is not "true" and
    # we stay silent rather than guess.
    [ "$(git -C "$cwd" rev-parse --is-inside-work-tree 2>/dev/null)" = "true" ] || return 0
    # `--git-common-dir` is the shared `.git` dir: for a worktree it points
    # at the MAIN repo's `.git`, so its parent is always the main repo root.
    common=$(git -C "$cwd" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) || return 0
    [ -n "$common" ] || return 0
    root=$(dirname "$common")
    case "$root" in
        "" | /) return 0 ;;
    esac
    basename "$root"
}

# Build a query-string suffix from "$1" plus any marker file walked up from
# it. Returns the suffix with the leading `&`, or nothing when cwd is absent.
# `cwd` is always included so `GET /handoff` resolves the same basename project
# as the prior hook events even when no marker file exists.
ai_memory_marker_qs() {
    cwd="$1"
    if [ -z "$cwd" ]; then
        ai_memory_managed_qs
        return 0
    fi
    qs="&cwd=$(ai_memory_url_encode "$cwd")"
    ws=""
    pr=""
    st=""
    ds=""
    marker=$(ai_memory_find_marker "$cwd")
    if [ -n "$marker" ]; then
        ws=$(ai_memory_parse_toml_key "$marker" workspace)
        pr=$(ai_memory_parse_toml_key "$marker" project)
        st=$(ai_memory_parse_toml_key "$marker" project_strategy)
        ds=$(ai_memory_parse_toml_key "$marker" drop_subagent_captures)
    fi
    # Install-time default baked into the hook command by
    # `install-hooks --project-strategy` fills the strategy only when no marker
    # pinned one. A marker's explicit project / project_strategy still win.
    if [ -z "$st" ] && [ -n "${AI_MEMORY_PROJECT_STRATEGY:-}" ]; then
        st="$AI_MEMORY_PROJECT_STRATEGY"
    fi
    # The repo-root strategy must be resolved here, on the host: a containerized
    # server cannot see this checkout, so its own libgit2 discovery fails and
    # falls back to basename(cwd). When repo-root is selected and no explicit
    # project is pinned, derive the main repo name now and send it as an explicit
    # `project` override. `project_strategy` is still forwarded so native servers
    # keep their existing resolution path.
    if [ -z "$pr" ]; then
        case "$st" in
            repo-root | repo_root) pr=$(ai_memory_repo_root_project "$cwd") ;;
        esac
    fi
    [ -n "$ws" ] && qs="${qs}&workspace=$(ai_memory_url_encode "$ws")"
    [ -n "$pr" ] && qs="${qs}&project=$(ai_memory_url_encode "$pr")"
    [ -n "$st" ] && qs="${qs}&project_strategy=$(ai_memory_url_encode "$st")"
    # Per-project drop_subagent_captures opt-in: forward to the server, which
    # interprets truthiness (1/true/...) and scopes the drop to this project.
    [ -n "$ds" ] && qs="${qs}&drop_subagent=$(ai_memory_url_encode "$ds")"
    qs="${qs}$(ai_memory_managed_qs)"
    printf '%s' "$qs"
}

# Build `&briefing=<v>[&briefing_budget=<v>]` from the `[briefing]` section
# of the marker walked up from "$1" (inject_on_session_start + optional
# max_chars). Prints nothing when cwd is absent or the repo did not opt in.
# NOT part of ai_memory_marker_qs on purpose: agents that deliver the brief
# once per session (kimi-code, via the first user prompt — kimi discards
# SessionStart hook stdout) append this only on the first fetch, so the
# server does not recompose the brief on every request. The char-budget clamp
# is decided server-side.
ai_memory_briefing_qs() {
    cwd="$1"
    [ -z "$cwd" ] && return 0
    marker=$(ai_memory_find_marker "$cwd")
    [ -n "$marker" ] || return 0
    qs=""
    briefing=$(ai_memory_parse_toml_flag "$marker" inject_on_session_start)
    case "$(printf '%s' "$briefing" | tr '[:upper:]' '[:lower:]')" in
        1|true|yes|on) ;;
        *) return 0 ;;
    esac
    budget=$(ai_memory_parse_toml_flag "$marker" max_chars)
    qs="&briefing=$(ai_memory_url_encode "$briefing")"
    [ -n "$budget" ] && qs="${qs}&briefing_budget=$(ai_memory_url_encode "$budget")"
    printf '%s' "$qs"
}

# Path of the once-per-session "brief delivered" marker for "$1" (a session
# id or a caller-built fallback key), sanitized to a safe file name under
# the shared state dir.
ai_memory_briefed_file() {
    key=$(printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '_')
    printf '%s/briefed/%s' "$(ai_memory_state_dir)" "$key"
}

# Write a once-per-session briefing marker and keep only the 512 newest
# markers. All marker names are sanitized by ai_memory_briefed_file.
ai_memory_mark_briefed() {
    path="$1"
    [ -n "$path" ] || return 0
    dir=$(dirname "$path")
    mkdir -p "$dir" 2>/dev/null || return 0
    : > "$path" 2>/dev/null || return 0
    LC_ALL=C ls -1t "$dir" 2>/dev/null \
        | sed -n '513,$p' \
        | while IFS= read -r stale; do
            [ -n "$stale" ] && rm -f "$dir/$stale" 2>/dev/null || true
        done
}

# Local bridge state for agents whose hook payloads do not carry a session id.
# The value is intentionally non-secret; the server hashes non-UUID ids into its
# typed SessionId domain. `AI_MEMORY_SESSION_ID` may be supplied by advanced
# launchers to pin an externally managed run id.
ai_memory_state_dir() {
    if [ -n "${AI_MEMORY_DATA_DIR:-}" ]; then
        printf '%s' "$AI_MEMORY_DATA_DIR"
    elif [ -n "${XDG_DATA_HOME:-}" ]; then
        printf '%s/ai-memory' "$XDG_DATA_HOME"
    elif [ -n "${HOME:-}" ]; then
        printf '%s/.local/share/ai-memory' "$HOME"
    else
        printf '.ai-memory'
    fi
}

ai_memory_session_id_file() {
    agent="$1"
    printf '%s/hook-state/%s-session-id' "$(ai_memory_state_dir)" "$agent"
}

ai_memory_new_session_id() {
    agent="$1"
    now=$(date +%s 2>/dev/null || printf '0')
    printf '%s-%s-%s' "$agent" "$now" "$$"
}

ai_memory_session_id_qs() {
    agent="$1"; event="$2"
    if [ -n "${AI_MEMORY_SESSION_ID:-}" ]; then
        printf '&session_id=%s' "$(ai_memory_url_encode "$AI_MEMORY_SESSION_ID")"
        return 0
    fi
    file=$(ai_memory_session_id_file "$agent")
    sid=""
    if [ "$event" != "session-start" ] && [ -f "$file" ]; then
        sid=$(sed -n '1p' "$file" 2>/dev/null)
    fi
    if [ -z "$sid" ]; then
        sid=$(ai_memory_new_session_id "$agent")
        dir=$(dirname "$file")
        mkdir -p "$dir" 2>/dev/null || true
        printf '%s\n' "$sid" > "$file" 2>/dev/null || true
    fi
    printf '&session_id=%s' "$(ai_memory_url_encode "$sid")"
}

ai_memory_clear_session_id() {
    agent="$1"
    rm -f "$(ai_memory_session_id_file "$agent")" 2>/dev/null || true
}

# POST stdin to "$1" as JSON, fire-and-forget. Adds an
# `Authorization: Bearer` header when `AI_MEMORY_AUTH_TOKEN` is set.
# The 0.5s timeout matches the project-wide hook latency budget
# (never block the agent), and the trailing `|| true` makes the
# function safe to call from `set -e` scripts.
ai_memory_post_hook() {
    if [ -n "${AI_MEMORY_AUTH_TOKEN:-}" ]; then
        curl -s --max-time 0.5 -X POST "$1" \
            -H "Content-Type: application/json" \
            -H "Authorization: Bearer $AI_MEMORY_AUTH_TOKEN" \
            --data-binary @-
    else
        curl -s --max-time 0.5 -X POST "$1" \
            -H "Content-Type: application/json" \
            --data-binary @-
    fi
}

# GET "$1" with the same auth-header rules as `ai_memory_post_hook`.
# Used by `session-start.sh` to pull the cross-agent handoff before
# the resuming agent's first prompt. 1s budget — slightly more
# generous than POST because the result is *synchronously* fed to
# stdout (and prepended to the agent's context), so we want to avoid
# truncating a handoff that was almost ready.
ai_memory_get_handoff() {
    if [ -n "${AI_MEMORY_AUTH_TOKEN:-}" ]; then
        curl -s --max-time 1.0 "$1" \
            -H "Authorization: Bearer $AI_MEMORY_AUTH_TOKEN"
    else
        curl -s --max-time 1.0 "$1"
    fi
}

# Encode stdin as a JSON string (with surrounding quotes). Used by hooks
# whose stdout contract is JSON rather than raw context text: Antigravity's
# PreInvocation hook and Claude Code's session-start hook (which wraps the
# handoff in hookSpecificOutput.additionalContext).
ai_memory_json_string() {
    awk '
        BEGIN { printf "\"" }
        {
            gsub(/\\/, "\\\\")
            gsub(/"/, "\\\"")
            gsub(/\t/, "\\t")
            gsub(/\r/, "\\r")
            printf "%s%s", sep, $0
            sep = "\\n"
        }
        END { printf "\"" }
    '
}
