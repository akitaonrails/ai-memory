#!/usr/bin/env bash
# install.sh refusals, run against crafted release tarballs in a temp dir:
# a symlink or hard-link member is refused before anything is unpacked, a
# plain release installs, the wget fallback pins https for the default
# GitHub base (refusing a wget that cannot), a binary that does not run is
# refused from beside the install directory, and a noexec TMPDIR does not
# refuse a working release. Needs no network and no root.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ai-memory-install-sh.XXXXXX")"
trap 'rm -rf "${TMP_ROOT}"' EXIT

TARGET=linux-x86_64
ASSET="ai-memory-${TARGET}.tar.gz"

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

# publish <name> <tree>: a release dir serving latest/download/<asset> and its
# checksum, built from <tree> the way release.yml builds it.
publish() {
  local name="$1" tree="$2" dir="${TMP_ROOT}/$1/release/latest/download"
  mkdir -p "${dir}"
  tar -C "${tree}" -czf "${dir}/${ASSET}" .
  printf '%s  %s\n' "$(sha256 "${dir}/${ASSET}")" "${ASSET}" >"${dir}/${ASSET}.sha256"
  printf '%s\n' "${TMP_ROOT}/${name}/release"
}

plain_tree() {
  local tree="$1"
  mkdir -p "${tree}/hooks"
  printf '#!/bin/sh\necho "ai-memory 0.0.0-test"\n' >"${tree}/ai-memory"
  chmod 0755 "${tree}/ai-memory"
  printf 'echo hook\n' >"${tree}/hooks/_lib.sh"
}

# run_install <case> <base-url> [env...]: install into a fresh home; output in
# <case>/out.log, exit status returned.
run_install() {
  local name="$1" base="$2"
  shift 2
  mkdir -p "${TMP_ROOT}/${name}/home"
  env HOME="${TMP_ROOT}/${name}/home" AI_MEMORY_INSTALL_TARGET="${TARGET}" \
    AI_MEMORY_RELEASE_BASE_URL="${base}" "$@" \
    sh "${ROOT}/install.sh" >"${TMP_ROOT}/${name}/out.log" 2>&1
}

# Control: a release of plain files and directories installs.
plain_tree "${TMP_ROOT}/plain-tree"
base="$(publish plain "${TMP_ROOT}/plain-tree")"
run_install plain "file://${base}" || fail "plain release refused: $(cat "${TMP_ROOT}/plain/out.log")"
[ -x "${TMP_ROOT}/plain/home/.local/lib/ai-memory/ai-memory" ] || fail "plain release not unpacked"
# The test PATH lacks ~/.local/bin, so the next step names the link by its full
# path; with the bin dir on PATH it is the bare command.
grep -Fq "next: ${TMP_ROOT}/plain/home/.local/bin/ai-memory setup" "${TMP_ROOT}/plain/out.log" \
  || fail "off-PATH next step not absolute: $(cat "${TMP_ROOT}/plain/out.log")"
base_on_path="$(publish on-path "${TMP_ROOT}/plain-tree")"
run_install on-path "file://${base_on_path}" PATH="${TMP_ROOT}/on-path/home/.local/bin:${PATH}" \
  || fail "on-PATH install refused: $(cat "${TMP_ROOT}/on-path/out.log")"
grep -Fxq "next: ai-memory setup" <(sed 's/^[^:]*: //' "${TMP_ROOT}/on-path/out.log") \
  || fail "on-PATH next step not the bare command: $(cat "${TMP_ROOT}/on-path/out.log")"

# The link must keep working from a different cwd when both overrides are
# relative to the directory that launched install.sh.
(
  cd "${TMP_ROOT}"
  run_install relative "file://${base}" \
    AI_MEMORY_BIN_DIR="relative/bin dir" AI_MEMORY_LIB_DIR="relative/lib dir"
) || fail "relative install refused: $(cat "${TMP_ROOT}/relative/out.log")"
relative_version=$("${TMP_ROOT}/relative/bin dir/ai-memory" --version) \
  || fail "relative install reported success but left an unusable link"
[ "${relative_version}" = "ai-memory 0.0.0-test" ] || fail "unexpected binary behind relative install link"

# A symlink member pointing outside the tree is refused, and nothing lands.
plain_tree "${TMP_ROOT}/symlink-tree"
ln -s /etc/passwd "${TMP_ROOT}/symlink-tree/hooks/escape"
base="$(publish symlink "${TMP_ROOT}/symlink-tree")"
if run_install symlink "file://${base}"; then
  fail "a release with a symlink member was installed"
fi
grep -Fq "not a plain file or directory" "${TMP_ROOT}/symlink/out.log" \
  || fail "symlink refusal not explained: $(cat "${TMP_ROOT}/symlink/out.log")"
[ ! -e "${TMP_ROOT}/symlink/home/.local/lib/ai-memory" ] || fail "a refused release was unpacked"

# A hard-link member is refused the same way.
plain_tree "${TMP_ROOT}/hardlink-tree"
ln "${TMP_ROOT}/hardlink-tree/hooks/_lib.sh" "${TMP_ROOT}/hardlink-tree/hooks/again.sh"
base="$(publish hardlink "${TMP_ROOT}/hardlink-tree")"
if run_install hardlink "file://${base}"; then
  fail "a release with a hard-link member was installed"
fi
grep -Fq "not a plain file or directory" "${TMP_ROOT}/hardlink/out.log" \
  || fail "hard-link refusal not explained: $(cat "${TMP_ROOT}/hardlink/out.log")"

# wget fallback: with no curl on PATH and the default GitHub base, wget runs
# with --https-only; a wget without that flag is refused before downloading.
tools="${TMP_ROOT}/tools"
mkdir -p "${tools}"
for tool in sh awk sed head tar gzip mktemp rm mkdir mv ln chmod dirname uname \
  cat grep sha256sum shasum openssl env printf; do
  path="$(command -v "${tool}" 2>/dev/null || true)"
  case "${path}" in /*) ln -sf "${path}" "${tools}/${tool}" ;; esac
done
plain_tree "${TMP_ROOT}/wget-tree"
wget_dir="$(publish wget "${TMP_ROOT}/wget-tree")/latest/download"
fake_wget() {
  local dir="$1" help="$2"
  mkdir -p "${dir}"
  cat >"${dir}/wget" <<WGET
#!/bin/sh
printf '%s\n' "\$*" >>"${dir}/wget.log"
if [ "\$1" = --help ]; then printf '%s\n' "${help}"; exit 0; fi
out=""; while [ \$# -gt 1 ]; do [ "\$1" = -O ] && out="\$2"; shift; done
cp "${wget_dir}/\$(basename "\$1")" "\${out}"
WGET
  chmod 0755 "${dir}/wget"
}
ln -sf "$(command -v cp)" "${tools}/cp"
ln -sf "$(command -v basename)" "${tools}/basename"
fake_wget "${TMP_ROOT}/wget-gnu" "  --https-only   only follow secure HTTPS links"
run_install wget-gnu "https://github.com/akitaonrails/ai-memory/releases" \
  PATH="${TMP_ROOT}/wget-gnu:${tools}" \
  || fail "GNU wget install failed: $(cat "${TMP_ROOT}/wget-gnu/out.log")"
grep -Fq -- "-q --https-only -O" "${TMP_ROOT}/wget-gnu/wget.log" \
  || fail "default-base wget download did not pin https: $(cat "${TMP_ROOT}/wget-gnu/wget.log")"

fake_wget "${TMP_ROOT}/wget-busybox" "BusyBox wget"
if run_install wget-busybox "https://github.com/akitaonrails/ai-memory/releases" \
  PATH="${TMP_ROOT}/wget-busybox:${tools}"; then
  fail "a wget without --https-only was trusted for the default base"
fi
grep -Fq "cannot refuse a redirect" "${TMP_ROOT}/wget-busybox/out.log" \
  || fail "busybox wget refusal not explained: $(cat "${TMP_ROOT}/wget-busybox/out.log")"
if grep -Fq -- " -O " "${TMP_ROOT}/wget-busybox/wget.log"; then
  fail "a refused wget still downloaded"
fi

# Control: an explicit mirror base keeps plain wget, as with curl.
fake_wget "${TMP_ROOT}/wget-mirror" "BusyBox wget"
run_install wget-mirror "http://mirror.invalid/releases" \
  PATH="${TMP_ROOT}/wget-mirror:${tools}" \
  || fail "mirror wget install failed: $(cat "${TMP_ROOT}/wget-mirror/out.log")"
if grep -Fq -- "--https-only" "${TMP_ROOT}/wget-mirror/wget.log"; then
  fail "an explicit mirror base must not force --https-only"
fi

# A work directory that refuses to execute, as /tmp does on a hardened host
# that mounts it noexec, must not reject a release that runs from its install
# location. Such a directory is borrowed rather than mounted: /run/lock is a
# world-writable noexec tmpfs on Debian and Ubuntu, and a probe script that
# fails to run from a candidate proves the refusal. Skipped where no candidate
# refuses (macOS, other layouts), so the case needs neither root nor a user
# namespace.
noexec_dir() {
  local candidate probe
  for candidate in /run/lock /dev/shm "${XDG_RUNTIME_DIR:-}"; do
    [ -n "${candidate}" ] && [ -d "${candidate}" ] && [ -w "${candidate}" ] || continue
    probe="$(mktemp "${candidate}/ai-memory-noexec.XXXXXX" 2>/dev/null)" || continue
    printf '#!/bin/sh\nexit 0\n' >"${probe}"
    chmod 0755 "${probe}"
    if "${probe}" 2>/dev/null; then
      rm -f "${probe}"
      continue
    fi
    rm -f "${probe}"
    printf '%s\n' "${candidate}"
    return 0
  done
  return 1
}
if noexec="$(noexec_dir)"; then
  plain_tree "${TMP_ROOT}/noexec-tree"
  base="$(publish noexec "${TMP_ROOT}/noexec-tree")"
  run_install noexec "file://${base}" TMPDIR="${noexec}" \
    || fail "install from a noexec TMPDIR (${noexec}) refused: $(cat "${TMP_ROOT}/noexec/out.log")"
  [ -x "${TMP_ROOT}/noexec/home/.local/lib/ai-memory/ai-memory" ] \
    || fail "install from a noexec TMPDIR unpacked nothing: $(cat "${TMP_ROOT}/noexec/out.log")"
  printf 'noexec TMPDIR case ran in %s\n' "${noexec}"
else
  printf 'noexec TMPDIR case skipped: no writable directory refuses to execute files\n'
fi

# Control: a release whose binary does not run is still refused, by the smoke
# test run from beside the install directory, and its staged copy is removed.
plain_tree "${TMP_ROOT}/broken-tree"
printf '#!/bin/sh\nexit 1\n' >"${TMP_ROOT}/broken-tree/ai-memory"
base="$(publish broken "${TMP_ROOT}/broken-tree")"
if run_install broken "file://${base}"; then
  fail "a release whose binary does not run was installed"
fi
grep -Fq "does not run from ${TMP_ROOT}/broken/home/.local/lib on this machine" \
  "${TMP_ROOT}/broken/out.log" \
  || fail "broken binary refusal not explained: $(cat "${TMP_ROOT}/broken/out.log")"
[ ! -e "${TMP_ROOT}/broken/home/.local/lib/ai-memory" ] || fail "a refused release was installed"
if ls -d "${TMP_ROOT}/broken/home/.local/lib/ai-memory.new."* >/dev/null 2>&1; then
  fail "a refused release left its staged copy behind"
fi

# A reinstall moves the lib dir aside and deletes it, so an AI_MEMORY_LIB_DIR
# naming the directory that holds the link is refused before anything changes:
# the link a first install left there passed for a release, every other program
# in that directory was deleted, and ai-memory became a link to itself.
plain_tree "${TMP_ROOT}/lib-is-bin-tree"
base="$(publish lib-is-bin "${TMP_ROOT}/lib-is-bin-tree")"
run_install lib-is-bin "file://${base}" || fail "first install refused: $(cat "${TMP_ROOT}/lib-is-bin/out.log")"
bin="${TMP_ROOT}/lib-is-bin/home/.local/bin"
printf '#!/bin/sh\necho mine\n' >"${bin}/my-tool"
chmod 0755 "${bin}/my-tool"
ln -s .local/bin "${TMP_ROOT}/lib-is-bin/home/bin"
for lib in "${bin}" "${bin}/" "${TMP_ROOT}/lib-is-bin/home/bin" "${TMP_ROOT}/lib-is-bin/home/.local"; do
  if run_install lib-is-bin "file://${base}" AI_MEMORY_LIB_DIR="${lib}"; then
    fail "AI_MEMORY_LIB_DIR=${lib} replaced the directory holding the link"
  fi
  grep -Fq "is or contains the link's directory" "${TMP_ROOT}/lib-is-bin/out.log" \
    || fail "refusal of AI_MEMORY_LIB_DIR=${lib} not explained: $(cat "${TMP_ROOT}/lib-is-bin/out.log")"
  [ "$("${bin}/my-tool")" = mine ] || fail "AI_MEMORY_LIB_DIR=${lib} deleted the user's program"
  [ "$("${bin}/ai-memory" --version)" = "ai-memory 0.0.0-test" ] \
    || fail "AI_MEMORY_LIB_DIR=${lib} broke the existing install"
done

# One directory for both, before either exists, ended with the same link to
# itself after a reported success.
fresh="${TMP_ROOT}/lib-is-bin-fresh/tools"
if run_install lib-is-bin-fresh "file://${base}" AI_MEMORY_BIN_DIR="${fresh}" AI_MEMORY_LIB_DIR="${fresh}"; then
  fail "a first install with one directory for the link and the release succeeded"
fi
grep -Fq "is or contains the link's directory" "${TMP_ROOT}/lib-is-bin-fresh/out.log" \
  || fail "one-directory refusal not explained: $(cat "${TMP_ROOT}/lib-is-bin-fresh/out.log")"
[ ! -e "${fresh}" ] || fail "a refused first install created ${fresh}"

# A lib dir is replaced only when it holds a release: the binary itself beside
# a hooks bundle. A package manager's bin dir (its ai-memory a link into the
# cellar), a directory whose ai-memory links to a hand-built one, and a
# wrapper's own directory (no hooks bundle) all stay as they were.
plain_tree "${TMP_ROOT}/not-a-release-tree"
base="$(publish not-a-release "${TMP_ROOT}/not-a-release-tree")"
home="${TMP_ROOT}/not-a-release/home"
mkdir -p "${home}/brew/Cellar" "${home}/brew/bin" "${home}/src/target" "${home}/apps/ai-memory/hooks" \
  "${home}/wrapper"
printf '#!/bin/sh\necho brewed\n' >"${home}/brew/Cellar/ai-memory"
printf '#!/bin/sh\necho hand-built\n' >"${home}/src/target/ai-memory"
printf '#!/bin/sh\necho wrapper\n' >"${home}/wrapper/ai-memory"
chmod 0755 "${home}/brew/Cellar/ai-memory" "${home}/src/target/ai-memory" "${home}/wrapper/ai-memory"
ln -s ../Cellar/ai-memory "${home}/brew/bin/ai-memory"
printf '#!/bin/sh\necho jq\n' >"${home}/brew/bin/jq"
ln -s ../../src/target/ai-memory "${home}/apps/ai-memory/ai-memory"
printf 'my hook\n' >"${home}/apps/ai-memory/hooks/mine.sh"
printf 'notes\n' >"${home}/wrapper/notes.txt"
for lib in "${home}/brew/bin" "${home}/apps/ai-memory" "${home}/wrapper"; do
  if run_install not-a-release "file://${base}" AI_MEMORY_LIB_DIR="${lib}"; then
    fail "AI_MEMORY_LIB_DIR=${lib} holds no release and was replaced"
  fi
  grep -Fq "holds no ai-memory release" "${TMP_ROOT}/not-a-release/out.log" \
    || fail "refusal of ${lib} not explained: $(cat "${TMP_ROOT}/not-a-release/out.log")"
done
[ -f "${home}/brew/bin/jq" ] || fail "the package manager's bin dir lost a program"
[ "$("${home}/brew/bin/ai-memory")" = brewed ] || fail "the package manager's link was replaced"
[ "$("${home}/apps/ai-memory/ai-memory")" = hand-built ] || fail "the link to a hand-built binary was replaced"
[ -f "${home}/apps/ai-memory/hooks/mine.sh" ] || fail "the linked binary's directory lost a file"
[ "$("${home}/wrapper/ai-memory")" = wrapper ] || fail "the user's wrapper was replaced"
[ -f "${home}/wrapper/notes.txt" ] || fail "the wrapper's directory lost a file"
[ ! -e "${home}/.local/bin/ai-memory" ] && [ ! -L "${home}/.local/bin/ai-memory" ] \
  || fail "a refused install created the link"

printf 'install.sh checks passed\n'
