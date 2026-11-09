#!/bin/sh
# Install the ai-memory release binary for the current user.
#
#   curl -fsSL https://github.com/akitaonrails/ai-memory/releases/latest/download/install.sh | sh
#
# Downloads ai-memory-<os>-<arch>.tar.gz from a GitHub release, verifies it
# against the release's published .sha256 file, unpacks it under
# ~/.local/lib/ai-memory and links ~/.local/bin/ai-memory to the binary. No
# sudo, and nothing outside those two directories is touched. A missing or
# mismatched checksum stops the install before anything is unpacked.
#
# Then run `ai-memory setup` to create the data dir, start the user service
# and wire your agents.
#
# Environment (all optional):
#   AI_MEMORY_VERSION           release to install, e.g. 3.0.0 (default: latest)
#   AI_MEMORY_BIN_DIR           where the `ai-memory` link goes (default: ~/.local/bin)
#   AI_MEMORY_LIB_DIR           where the release is unpacked
#                               (default: ~/.local/lib/ai-memory); every install
#                               replaces it whole, so it must be a directory of
#                               its own, never the bin dir or one above it
#   AI_MEMORY_RELEASE_BASE_URL  Releases-compatible URL prefix, the one
#                               `ai-memory upgrade` reads (default: GitHub).
#                               The checksum comes from the same place as the
#                               archive, so point it only at an origin you trust.
#   AI_MEMORY_INSTALL_TARGET    force a target: linux-x86_64, linux-aarch64,
#                               macos-x86_64 or macos-aarch64
#
# Everything runs inside main(), called on the last line, so a download cut
# short runs nothing.

set -eu

DEFAULT_RELEASE_BASE="https://github.com/akitaonrails/ai-memory/releases"

say() {
    printf 'ai-memory install: %s\n' "$*"
}

die() {
    printf 'ai-memory install: error: %s\n' "$*" >&2
    exit 1
}

detect_target() {
    if [ -n "${AI_MEMORY_INSTALL_TARGET:-}" ]; then
        case "$AI_MEMORY_INSTALL_TARGET" in
            linux-x86_64 | linux-aarch64 | macos-x86_64 | macos-aarch64)
                printf '%s\n' "$AI_MEMORY_INSTALL_TARGET"
                return 0
                ;;
            *) die "AI_MEMORY_INSTALL_TARGET must be linux-x86_64, linux-aarch64, macos-x86_64 or macos-aarch64" ;;
        esac
    fi
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Linux) os=linux ;;
        Darwin) os=macos ;;
        MINGW* | MSYS* | CYGWIN* | Windows_NT)
            die "Windows is not supported by this script; in PowerShell run: irm https://github.com/akitaonrails/ai-memory/releases/latest/download/install.ps1 | iex"
            ;;
        *) die "unsupported operating system: $os" ;;
    esac
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) die "unsupported architecture: $arch" ;;
    esac
    # An x86_64 shell under Rosetta on Apple Silicon still wants the native build.
    if [ "$os" = macos ] && [ "$arch" = x86_64 ] &&
        [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
        arch=aarch64
    fi
    printf '%s-%s\n' "$os" "$arch"
}

# download <url> <dest>: fails on any HTTP error, so a missing file never
# leaves a page of HTML behind.
download() {
    if command -v curl >/dev/null 2>&1; then
        if [ "$release_base" = "$DEFAULT_RELEASE_BASE" ]; then
            curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"
        else
            curl -fsSL -o "$2" "$1"
        fi
    elif command -v wget >/dev/null 2>&1; then
        if [ "$release_base" = "$DEFAULT_RELEASE_BASE" ]; then
            # The curl path's --proto '=https' for wget: no redirect may drop
            # to plain http. A wget without the flag (BusyBox) cannot promise
            # that, so it is refused rather than trusted.
            wget --help 2>&1 | grep -q -- '--https-only' ||
                die "this wget cannot refuse a redirect to plain http (no --https-only); install curl or GNU wget"
            wget -q --https-only -O "$2" "$1"
        else
            wget -q -O "$2" "$1"
        fi
    else
        die "curl or wget is required"
    fi
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | awk '{print $NF}'
    else
        die "no SHA-256 tool found (sha256sum, shasum or openssl)"
    fi
}

# verify <archive> <checksum-file> <asset-name>
verify() {
    # The sidecar is "<hex>  <asset>": one line, for exactly this asset.
    line="$(head -n 1 "$2")"
    expected="$(printf '%s\n' "$line" | awk '{print $1}')"
    named="$(printf '%s\n' "$line" | awk '{print $2}' | sed 's/^\*//')"
    case "$expected" in
        *[!0-9a-f]* | "") die "checksum file for $3 is malformed; refusing installation" ;;
    esac
    [ "${#expected}" -eq 64 ] || die "checksum file for $3 is malformed; refusing installation"
    [ "$named" = "$3" ] || die "checksum file names '$named', not $3; refusing installation"
    actual="$(sha256_of "$1")"
    [ "$actual" = "$expected" ] || die "checksum mismatch for $3 (expected $expected, got $actual); refusing installation"
}

# Refuse an archive with an absolute or parent-relative member, or with any
# member that is not a plain file or directory (a symlink or hard link could
# point the unpacked tree outside itself), before tar writes anything.
check_members() {
    tar -tzf "$1" >"$2" || die "cannot read $3"
    while IFS= read -r member; do
        case "$member" in
            /* | ../* | */../* | .. | */..) die "$3 contains an unsafe path ($member); refusing installation" ;;
        esac
    done <"$2"
    # GNU tar and bsdtar both start each verbose line with the member type.
    tar -tvzf "$1" >"$2" || die "cannot read $3"
    while IFS= read -r entry; do
        case "$entry" in
            -* | d*) ;;
            *) die "$3 contains a member that is not a plain file or directory ($entry); refusing installation" ;;
        esac
    done <"$2"
}

# real_dir <dir>: the physical path of an existing directory, symlinks
# resolved, or nothing when it does not exist.
real_dir() {
    (cd -P -- "$1" 2>/dev/null && pwd -P) || true
}

# dir_contains <dir> <path>: <path> is <dir> or lies below it. Two spellings of
# one existing directory (a trailing slash, a symlinked parent) compare equal by
# their physical paths; a part that does not exist yet compares as written.
dir_contains() {
    case "$2" in
        /*) path="$2" ;;
        *) path="$(pwd -P)/$2" ;;
    esac
    real="$(real_dir "$1")"
    while :; do
        if [ "${path%/}" = "${1%/}" ]; then
            return 0
        fi
        if [ -n "$real" ] && [ "$(real_dir "$path")" = "$real" ]; then
            return 0
        fi
        parent="$(dirname "$path")"
        [ "$parent" != "$path" ] || return 1
        path="$parent"
    done
}

# links_to <link> <file>: <link> is a symlink to <file>, as written or as
# another spelling of the same existing path (a symlinked parent directory).
links_to() {
    to="$(readlink "$1")" || return 1
    [ "$to" != "$2" ] || return 0
    case "$to" in
        /*) ;;
        *) to="$(dirname "$1")/$to" ;;
    esac
    [ "${to##*/}" = "${2##*/}" ] || return 1
    from="$(real_dir "$(dirname "$to")")"
    [ -n "$from" ] && [ "$from" = "$(real_dir "$(dirname "$2")")" ]
}

# check_replaceable <lib-dir> <release-tree>: a reinstall deletes the whole
# lib dir, so every entry in it must also be in the release replacing it, or be
# one that `ai-memory upgrade` leaves while it swaps the binary and hooks/ in
# place (`ai-memory.new`, `.hooks.new`, `.hooks.old`). A file the user keeps
# there would go with the directory.
check_replaceable() {
    for entry in "$1"/* "$1"/.[!.]* "$1"/..?*; do
        [ -e "$entry" ] || [ -L "$entry" ] || continue
        name="${entry##*/}"
        case "$name" in
            ai-memory.new | .hooks.new | .hooks.old) continue ;;
        esac
        [ -e "$2/$name" ] ||
            die "$1 holds $name, which the release does not ship and a reinstall would delete; move it aside or set AI_MEMORY_LIB_DIR"
    done
}

main() {
    home="${HOME:-}"
    [ -n "$home" ] || die "HOME is not set"
    bin_dir="${AI_MEMORY_BIN_DIR:-$home/.local/bin}"
    lib_dir="${AI_MEMORY_LIB_DIR:-$home/.local/lib/ai-memory}"
    release_base="${AI_MEMORY_RELEASE_BASE_URL:-$DEFAULT_RELEASE_BASE}"
    release_base="${release_base%/}"
    version="${AI_MEMORY_VERSION:-latest}"

    # A relative link target is resolved from the link's directory.
    case "$lib_dir" in
        /*) ;;
        *) lib_dir="$(pwd -P)/$lib_dir" ;;
    esac

    case "$bin_dir$lib_dir" in
        *"
"*) die "install paths must not contain a newline" ;;
    esac
    case "$version" in
        latest) url_dir="$release_base/latest/download" ;;
        *[!0-9A-Za-z.+-]*) die "AI_MEMORY_VERSION must look like 3.0.0" ;;
        v*) url_dir="$release_base/download/$version" ;;
        *) url_dir="$release_base/download/v$version" ;;
    esac

    target="$(detect_target)"
    asset="ai-memory-$target.tar.gz"
    link="$bin_dir/ai-memory"

    # A reinstall moves the lib dir aside and deletes it, so the bin dir, or
    # any directory above it, would take every other program there with it
    # and leave the link pointing at itself.
    if dir_contains "$lib_dir" "$bin_dir"; then
        die "$lib_dir is or contains the link's directory $bin_dir, and every install replaces the lib dir whole; set AI_MEMORY_LIB_DIR to a directory of its own"
    fi

    # Never replace something this script did not put there: a Docker wrapper,
    # a hand-built binary, or a link to either at the same path is the user's.
    if [ -e "$link" ] || [ -L "$link" ]; then
        [ -L "$link" ] || die "$link already exists and is not a link to an install.sh release; move it aside or set AI_MEMORY_BIN_DIR"
        links_to "$link" "$lib_dir/ai-memory" ||
            die "$link links to $(readlink "$link"), not to this install's $lib_dir/ai-memory; move it aside or set AI_MEMORY_BIN_DIR"
    fi
    # An existing lib dir is replaced only when it holds a release: the binary
    # itself, not a link to another one, beside the hooks bundle every release
    # ships. A package manager's bin dir, or a wrapper's own directory, holds
    # an `ai-memory` too and is the user's.
    if [ -e "$lib_dir" ]; then
        if [ ! -f "$lib_dir/ai-memory" ] || [ -L "$lib_dir/ai-memory" ] ||
            [ ! -d "$lib_dir/hooks" ]; then
            die "$lib_dir exists but holds no ai-memory release; move it aside or set AI_MEMORY_LIB_DIR"
        fi
    fi

    work="$(mktemp -d "${TMPDIR:-/tmp}/ai-memory-install.XXXXXX")"
    trap 'rm -rf "$work"' EXIT
    trap 'exit 1' INT TERM

    say "downloading $asset ($version)"
    download "$url_dir/$asset" "$work/$asset" || die "could not download $url_dir/$asset"
    download "$url_dir/$asset.sha256" "$work/$asset.sha256" ||
        die "could not download $url_dir/$asset.sha256; refusing installation without a checksum"
    verify "$work/$asset" "$work/$asset.sha256" "$asset"
    say "checksum verified"

    check_members "$work/$asset" "$work/members" "$asset"
    mkdir -p "$work/release"
    tar --no-same-owner -xzf "$work/$asset" -C "$work/release"
    [ -f "$work/release/ai-memory" ] || die "$asset has no ai-memory binary"
    chmod 0755 "$work/release/ai-memory"
    if [ -e "$lib_dir" ]; then
        check_replaceable "$lib_dir" "$work/release"
    fi

    mkdir -p "$(dirname "$lib_dir")" "$bin_dir"
    staged="$lib_dir.new.$$"
    rm -rf "$staged"
    mv "$work/release" "$staged"
    # Run the binary from beside its install directory, not from the work
    # directory: a TMPDIR mounted noexec refuses to execute anything there,
    # which would reject a release that runs where it is installed.
    if ! "$staged/ai-memory" --version >/dev/null 2>&1; then
        rm -rf "$staged"
        die "the downloaded binary does not run from $(dirname "$lib_dir") on this machine"
    fi
    if [ -e "$lib_dir" ]; then
        previous="$lib_dir.old.$$"
        mv "$lib_dir" "$previous"
        if ! mv "$staged" "$lib_dir"; then
            mv "$previous" "$lib_dir"
            die "could not move the new release into $lib_dir; the previous one was kept"
        fi
        rm -rf "$previous"
    else
        mv "$staged" "$lib_dir"
    fi
    rm -f "$link"
    ln -s "$lib_dir/ai-memory" "$link"

    say "installed $("$link" --version 2>/dev/null || echo ai-memory) to $lib_dir"
    say "linked $link"
    # Off PATH, a bare `ai-memory setup` would fail with "command not found",
    # so the next step names the link itself.
    case ":${PATH:-}:" in
        *":$bin_dir:"*) say "next: ai-memory setup" ;;
        *)
            say "$bin_dir is not on PATH; add it, e.g. export PATH=\"$bin_dir:\$PATH\""
            say "next: $link setup"
            ;;
    esac
}

main "$@"
