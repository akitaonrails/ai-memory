#!/usr/bin/env bash
# CI-safe checks for the native Linux package assets.
#
# This script must not mutate host-level /usr, /etc, /var, users, groups, or
# services. It validates systemd/sysusers/tmpfiles behavior against a temporary
# alternate root and removes that root on exit.

set -euo pipefail

TMP_ROOT=""

log() {
  printf '==> %s\n' "$*"
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

repo_root() {
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  cd "${script_dir}/.." && pwd
}

require_tool() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is required"
}

copy_system_unit_dependency() {
  local unit="$1"
  local dest="$2/usr/lib/systemd/system/$unit"
  local source_dir
  for source_dir in /usr/lib/systemd/system /lib/systemd/system; do
    if [ -f "${source_dir}/${unit}" ]; then
      cp "${source_dir}/${unit}" "${dest}"
      return 0
    fi
  done
}

assert_contains() {
  local file="$1"
  local needle="$2"
  if ! grep -Fq -- "$needle" "$file"; then
    fail "${file} does not contain expected text: ${needle}"
  fi
}

main() {
  require_tool systemd-analyze
  require_tool systemd-sysusers
  require_tool systemd-tmpfiles

  local repo tmpfiles_output
  repo="$(repo_root)"
  cd "${repo}"

  log "Checking shell packaging syntax"
  bash -n packaging/aur/PKGBUILD
  bash -n packaging/aur/PKGBUILD-bin
  bash -n packaging/aur/ai-memory.install
  bash -n bin/ai-memory
  bash -n scripts/test-native-arch-systemd-distrobox.sh

  if command -v makepkg >/dev/null 2>&1 && [ "$(id -u)" != "0" ]; then
    log "Checking AUR .SRCINFO generation"
    (cd packaging/aur && makepkg --printsrcinfo -p PKGBUILD) >/dev/null
    (cd packaging/aur && makepkg --printsrcinfo -p PKGBUILD-bin) >/dev/null
  else
    log "Skipping makepkg .SRCINFO check (makepkg unavailable or running as root)"
  fi

  TMP_ROOT="$(mktemp -d /tmp/ai-memory-native-root.XXXXXX)"
  cleanup() {
    if [ -n "${TMP_ROOT}" ]; then
      rm -rf "${TMP_ROOT}"
    fi
  }
  trap cleanup EXIT

  log "Checking host-launch wrapper routing"
  local fake_docker fake_native wrapper_log
  fake_docker="${TMP_ROOT}/forbidden-docker"
  fake_native="${TMP_ROOT}/fake-ai-memory"
  wrapper_log="${TMP_ROOT}/wrapper.log"
  printf '%s\n' '#!/usr/bin/env bash' 'exit 97' >"${fake_docker}"
  printf '%s\n' '#!/usr/bin/env bash' 'printf '\''%s\n'\'' "$*" >>"${AI_MEMORY_WRAPPER_TEST_LOG}"' >"${fake_native}"
  chmod 0755 "${fake_docker}" "${fake_native}"
  AI_MEMORY_DOCKER="${fake_docker}" AI_MEMORY_NATIVE_BIN="${fake_native}" \
    AI_MEMORY_WRAPPER_TEST_LOG="${wrapper_log}" \
    bin/ai-memory run codex --yolo
  AI_MEMORY_DOCKER="${fake_docker}" AI_MEMORY_NATIVE_BIN="${fake_native}" \
    AI_MEMORY_WRAPPER_TEST_LOG="${wrapper_log}" \
    bin/ai-memory show --json --no-scan
  AI_MEMORY_DOCKER="${fake_docker}" AI_MEMORY_NATIVE_BIN="${fake_native}" \
    AI_MEMORY_WRAPPER_TEST_LOG="${wrapper_log}" \
    bin/ai-memory continue --workspace work --yolo
  AI_MEMORY_DOCKER="${fake_docker}" AI_MEMORY_NATIVE_BIN="${fake_native}" \
    AI_MEMORY_WRAPPER_TEST_LOG="${wrapper_log}" \
    bin/ai-memory workstreams --limit 5 --json
  AI_MEMORY_DOCKER="${fake_docker}" AI_MEMORY_NATIVE_BIN="${fake_native}" \
    AI_MEMORY_WRAPPER_TEST_LOG="${wrapper_log}" \
    bin/ai-memory rename-workstream --from typo-nmae --to refactor-db
  assert_contains "${wrapper_log}" "run codex --yolo"
  assert_contains "${wrapper_log}" "show --json --no-scan"
  assert_contains "${wrapper_log}" "continue --workspace work --yolo"
  assert_contains "${wrapper_log}" "workstreams --limit 5 --json"
  assert_contains "${wrapper_log}" "rename-workstream --from typo-nmae --to refactor-db"

  log "Creating temporary alternate root"
  mkdir -p \
    "${TMP_ROOT}/etc" \
    "${TMP_ROOT}/etc/ai-memory" \
    "${TMP_ROOT}/usr/bin" \
    "${TMP_ROOT}/usr/lib/systemd/system" \
    "${TMP_ROOT}/usr/lib/systemd/user" \
    "${TMP_ROOT}/usr/lib/sysusers.d" \
    "${TMP_ROOT}/usr/lib/tmpfiles.d" \
    "${TMP_ROOT}/var/lib"
  : >"${TMP_ROOT}/etc/passwd"
  : >"${TMP_ROOT}/etc/group"
  : >"${TMP_ROOT}/usr/bin/ai-memory"
  chmod 0755 "${TMP_ROOT}/usr/bin/ai-memory"

  cp crates/ai-memory-cli/templates/config.default.toml "${TMP_ROOT}/etc/ai-memory/config.toml"
  cp packaging/env/ai-memory.env "${TMP_ROOT}/etc/ai-memory/env"
  chmod 0640 "${TMP_ROOT}/etc/ai-memory/env"
  cp packaging/systemd/ai-memory.service "${TMP_ROOT}/usr/lib/systemd/system/ai-memory.service"
  cp packaging/systemd/ai-memory-user.service "${TMP_ROOT}/usr/lib/systemd/user/ai-memory.service"
  cp packaging/sysusers/ai-memory.conf "${TMP_ROOT}/usr/lib/sysusers.d/ai-memory.conf"
  cp packaging/tmpfiles/ai-memory.conf "${TMP_ROOT}/usr/lib/tmpfiles.d/ai-memory.conf"

  for unit in \
    sysinit.target \
    basic.target \
    multi-user.target \
    network-online.target \
    sockets.target \
    timers.target \
    paths.target \
    slices.target \
    shutdown.target \
    remote-fs.target \
    local-fs.target \
    swap.target; do
    copy_system_unit_dependency "$unit" "$TMP_ROOT" || true
  done

  log "Checking sysusers in alternate root"
  systemd-sysusers --root="${TMP_ROOT}" "${TMP_ROOT}/usr/lib/sysusers.d/ai-memory.conf" >/dev/null
  assert_contains "${TMP_ROOT}/etc/passwd" "ai-memory service user:/var/lib/ai-memory:/usr/bin/nologin"
  assert_contains "${TMP_ROOT}/etc/group" "ai-memory"

  log "Checking tmpfiles in alternate root"
  if systemd-tmpfiles --help 2>&1 | grep -q -- '--dry-run'; then
    tmpfiles_output="$(systemd-tmpfiles --root="${TMP_ROOT}" --create --dry-run "${TMP_ROOT}/usr/lib/tmpfiles.d/ai-memory.conf" 2>&1)"
    case "${tmpfiles_output}" in
      *"/var/lib/ai-memory"*) ;;
      *) fail "tmpfiles dry-run did not plan /var/lib/ai-memory: ${tmpfiles_output}" ;;
    esac
  elif [ "$(id -u)" = "0" ]; then
    systemd-tmpfiles --root="${TMP_ROOT}" --create "${TMP_ROOT}/usr/lib/tmpfiles.d/ai-memory.conf" >/dev/null
    test -d "${TMP_ROOT}/var/lib/ai-memory" || fail "tmpfiles did not create /var/lib/ai-memory in alternate root"
    test "$(stat -c '%a' "${TMP_ROOT}/var/lib/ai-memory")" = "750" || fail "tmpfiles created /var/lib/ai-memory with unexpected mode"
  else
    tmpfiles_output="$(systemd-tmpfiles --root="${TMP_ROOT}" --cat-config "${TMP_ROOT}/usr/lib/tmpfiles.d/ai-memory.conf" 2>&1)"
    case "${tmpfiles_output}" in
      *"/var/lib/ai-memory"*) ;;
      *) fail "tmpfiles config parse did not include /var/lib/ai-memory: ${tmpfiles_output}" ;;
    esac
  fi
  assert_contains packaging/tmpfiles/ai-memory.conf "d /var/lib/ai-memory 0750 ai-memory ai-memory -"

  log "Checking systemd units in alternate root"
  systemd-analyze --root="${TMP_ROOT}" verify ai-memory.service

  # systemd-analyze cannot combine --user and --root on some distro versions.
  # Copy the user unit into the system search path under a temporary name to
  # still parse and validate the Service/Install directives with the same fake
  # /usr/bin/ai-memory executable.
  cp packaging/systemd/ai-memory-user.service \
    "${TMP_ROOT}/usr/lib/systemd/system/ai-memory-user-parse-test.service"
  systemd-analyze --root="${TMP_ROOT}" verify ai-memory-user-parse-test.service

  log "Checking expected native paths and modes"
  assert_contains packaging/systemd/ai-memory.service "--data-dir /var/lib/ai-memory"
  assert_contains packaging/systemd/ai-memory.service "--config /etc/ai-memory/config.toml"
  assert_contains packaging/systemd/ai-memory.service "EnvironmentFile=-/etc/ai-memory/env"
  assert_contains packaging/systemd/ai-memory.service "StateDirectory=ai-memory"
  assert_contains packaging/systemd/ai-memory.service "ReadWritePaths=/var/lib/ai-memory"
  assert_contains packaging/systemd/ai-memory-user.service "--data-dir %h/.local/share/ai-memory"
  assert_contains packaging/systemd/ai-memory-user.service "--config %h/.config/ai-memory/config.toml"
  assert_contains packaging/systemd/ai-memory-user.service "EnvironmentFile=-%h/.config/ai-memory/env"
  test "$(stat -c '%a' "${TMP_ROOT}/etc/ai-memory/env")" = "640"

  log "Native packaging checks passed without touching host service paths"
}

main "$@"
