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

# The TypeScript SDK in companions/ai-memory-ts is optional and never ships
# with the server. Release tarballs, Homebrew, the Docker image, AUR, RPM and
# Nix build the root Cargo workspace and copy hooks/ only. Fail if any of
# their build files starts referencing the SDK.
SERVER_DISTRIBUTION_FILES=(
  .github/workflows/release.yml
  bin/release
  docker/Dockerfile
  packaging/aur/PKGBUILD
  packaging/aur/PKGBUILD-bin
  packaging/rpm/ai-memory.spec
  flake.nix
  nix/nixos-module.nix
  nix/settings-options.nix
  nix/systemd-sandbox.nix
)

assert_no_sdk_reference() {
  local file hits
  for file in "$@"; do
    [ -f "${file}" ] || fail "server distribution file ${file} is missing; update SERVER_DISTRIBUTION_FILES"
    hits="$(grep -n 'ai-memory-ts' "${file}" || true)"
    if [ -n "${hits}" ]; then
      fail "${file} references the optional TypeScript SDK, which must stay out of the server distribution: ${hits}"
    fi
  done
}

check_sdk_not_distributed() {
  log "Checking the optional TypeScript SDK stays out of the server distribution"
  assert_no_sdk_reference "${SERVER_DISTRIBUTION_FILES[@]}"
  # Prove the guard bites: a distribution file that names the SDK must fail.
  local probe
  probe="$(mktemp)"
  printf 'COPY companions/ai-memory-ts /opt/sdk\n' >"${probe}"
  if (assert_no_sdk_reference "${probe}") 2>/dev/null; then
    rm -f "${probe}"
    fail "the SDK distribution guard did not catch a reference"
  fi
  rm -f "${probe}"
}

# packaging/scoop/ai-memory.json is a template: the release workflow pins its
# version, url and hash to the tag before pushing it to the Scoop bucket. Check
# the parts the workflow keeps as they are, and that they name the zip the
# release actually builds.
check_scoop_manifest() {
  local manifest=packaging/scoop/ai-memory.json
  local zip=ai-memory-windows-x86_64.zip
  log "Checking Scoop manifest template"
  require_tool jq
  jq -e . "${manifest}" >/dev/null || fail "${manifest} is not valid JSON"
  jq -e --arg zip "${zip}" '
    . as $m
    | (.version | test("^[0-9]+\\.[0-9]+\\.[0-9]+$"))
    and .homepage == "https://aimemory.sh"
    and .license == "MIT"
    and .bin == "ai-memory.exe"
    and .checkver.github == "https://github.com/akitaonrails/ai-memory"
    and (.architecture."64bit".url | endswith("/v" + $m.version + "/" + $zip))
    and (.architecture."64bit".hash | test("^[0-9a-f]{64}$"))
    and .autoupdate.architecture."64bit".url
      == "https://github.com/akitaonrails/ai-memory/releases/download/v$version/" + $zip
    and .autoupdate.hash.url == "$url.sha256"
  ' "${manifest}" >/dev/null \
    || fail "${manifest} does not match the release's Windows zip or Scoop's manifest fields"
  grep -Fq "RELEASE_ARTIFACT: ${zip%.zip}" .github/workflows/release.yml \
    || fail "release.yml no longer builds ${zip}, which ${manifest} installs"
}

main() {
  local repo
  repo="$(repo_root)"
  cd "${repo}"
  check_sdk_not_distributed

  require_tool systemd-analyze
  require_tool systemd-sysusers
  require_tool systemd-tmpfiles

  local tmpfiles_output

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

  if command -v rpmspec >/dev/null 2>&1; then
    log "Checking RPM spec syntax"
    rpmspec --parse --define '_arch x86_64' packaging/rpm/ai-memory.spec >/dev/null
  else
    log "Skipping RPM spec check (rpmspec unavailable)"
  fi

  check_scoop_manifest

  # Honor TMPDIR: mktemp ignores it once the template carries an explicit
  # directory, and a host whose /tmp is small or quota-limited would reject
  # the writes below.
  TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ai-memory-native-root.XXXXXX")"
  cleanup() {
    if [ -n "${TMP_ROOT}" ]; then
      rm -rf "${TMP_ROOT}"
    fi
  }
  trap cleanup EXIT

  log "Checking host-launch wrapper routing"
  local fake_docker fake_compat_docker fake_native wrapper_log docker_log native_lines
  fake_docker="${TMP_ROOT}/forbidden-docker"
  fake_compat_docker="${TMP_ROOT}/fake-compat-docker"
  fake_native="${TMP_ROOT}/fake-ai-memory"
  wrapper_log="${TMP_ROOT}/wrapper.log"
  docker_log="${TMP_ROOT}/docker.log"
  printf '%s\n' '#!/usr/bin/env bash' 'exit 97' >"${fake_docker}"
  printf '%s\n' '#!/usr/bin/env bash' 'printf '\''%s\n'\'' "$*" >>"${AI_MEMORY_DOCKER_TEST_LOG}"' >"${fake_compat_docker}"
  printf '%s\n' '#!/usr/bin/env bash' 'printf '\''%s\n'\'' "$*" >>"${AI_MEMORY_WRAPPER_TEST_LOG}"' >"${fake_native}"
  chmod 0755 "${fake_docker}" "${fake_compat_docker}" "${fake_native}"
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
  AI_MEMORY_DOCKER="${fake_docker}" AI_MEMORY_NATIVE_BIN="${fake_native}" \
    AI_MEMORY_WRAPPER_TEST_LOG="${wrapper_log}" \
    bin/ai-memory install-hooks --agent claude-code --capture-mode allowlist --apply
  AI_MEMORY_DOCKER="${fake_docker}" AI_MEMORY_NATIVE_BIN="${fake_native}" \
    AI_MEMORY_WRAPPER_TEST_LOG="${wrapper_log}" \
    bin/ai-memory --config '/tmp/ai memory.toml' run codex --yolo
  AI_MEMORY_DOCKER="${fake_docker}" AI_MEMORY_NATIVE_BIN="${fake_native}" \
    AI_MEMORY_WRAPPER_TEST_LOG="${wrapper_log}" \
    bin/ai-memory --data-dir=/tmp/ai-memory show --json
  assert_contains "${wrapper_log}" "--config /tmp/ai memory.toml run codex --yolo"
  assert_contains "${wrapper_log}" "--data-dir=/tmp/ai-memory show --json"
  assert_contains "${wrapper_log}" "run codex --yolo"
  assert_contains "${wrapper_log}" "show --json --no-scan"
  assert_contains "${wrapper_log}" "continue --workspace work --yolo"
  assert_contains "${wrapper_log}" "workstreams --limit 5 --json"
  assert_contains "${wrapper_log}" "rename-workstream --from typo-nmae --to refactor-db"
  assert_contains "${wrapper_log}" "install-hooks --agent claude-code --capture-mode allowlist --apply"

  native_lines="$(wc -l <"${wrapper_log}")"
  AI_MEMORY_DOCKER="${fake_compat_docker}" AI_MEMORY_NATIVE_BIN="${fake_native}" \
    AI_MEMORY_HOOK_PLATFORM=posix AI_MEMORY_DOCKER_TEST_LOG="${docker_log}" \
    AI_MEMORY_WRAPPER_TEST_LOG="${wrapper_log}" \
    bin/ai-memory install-hooks --agent claude-code --apply
  assert_contains "${docker_log}" "install-hooks --agent claude-code --apply"
  test "$(wc -l <"${wrapper_log}")" = "${native_lines}" \
    || fail "an explicit posix hook install must retain the container/script path"

  if AI_MEMORY_DOCKER="${fake_docker}" AI_MEMORY_NATIVE_BIN="$(pwd)/bin/ai-memory" \
    bin/ai-memory run codex 2>/dev/null; then
    fail "AI_MEMORY_NATIVE_BIN pointing to the wrapper must be refused instead of recursing"
  fi

  log "Checking the PowerShell wrapper intercepts the same native host commands"
  # bin/ai-memory.ps1 routes run/show/continue/resume/workstreams/rename-
  # workstream to the native Windows client (#1545). Its list must match the
  # POSIX wrapper's intercept exactly, and both must point a missing client
  # at the documented Windows installer. Static only: this script has no pwsh.
  local posix_list ps1_list
  posix_list="$(grep -m1 '^  run | show | continue | resume | workstreams | rename-workstream)' bin/ai-memory \
    | sed 's/^ *//; s/)$//' | tr '|' ' ' | tr -s ' ' '\n' | sort | paste -sd' ' -)"
  ps1_list="$(sed -n '/^\$NativeHostCommands = @($/,/^)$/p' bin/ai-memory.ps1 \
    | grep -o '"[a-z-]*"' | tr -d '"' | sort | paste -sd' ' -)"
  test -n "${posix_list}" || fail "could not extract the native host command list from bin/ai-memory"
  test "${posix_list}" = "${ps1_list}" \
    || fail "native host command lists drifted between bin/ai-memory (${posix_list}) and bin/ai-memory.ps1 (${ps1_list})"
  assert_contains bin/ai-memory.ps1 "https://github.com/akitaonrails/ai-memory/releases/latest/download/install.ps1"
  assert_contains bin/ai-memory "https://github.com/akitaonrails/ai-memory/releases/latest/download/install.ps1"

  log "Checking the wrapper hands host commands the volume's root bearer"
  # New-install default: a server initialized on the named volume keeps its token at
  # /data/auth-tokens/default. Host-native commands must receive it as a
  # 0600 token file named by AI_MEMORY_AUTH__BEARER_TOKEN_FILE, never in argv.
  local token_docker token_native token_log token_docker_log token_home bind_dir
  token_docker="${TMP_ROOT}/token-docker"
  token_native="${TMP_ROOT}/token-native"
  token_log="${TMP_ROOT}/token-native.log"
  token_docker_log="${TMP_ROOT}/token-docker.log"
  token_home="${TMP_ROOT}/token-home"
  bind_dir="${TMP_ROOT}/bind-data"
  mkdir -p "${token_home}" "${bind_dir}"
  # An explicit env token wins over the volume copy, so a contributor shell
  # that exports one would hide the hand-off this section checks.
  unset AI_MEMORY_AUTH_TOKEN
  cat >"${token_docker}" <<'FAKE'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${AI_MEMORY_DOCKER_TEST_LOG}"
case "$*" in
  *"--entrypoint /bin/sh"*)
    [ -z "${AI_MEMORY_TEST_ENGINE_DOWN:-}" ] || exit 125
    printf '%s\n' "${AI_MEMORY_TEST_VOLUME_TOKEN-VOLUME-ROOT-TOKEN}"
    ;;
  *" uninstall "* | *" uninstall")
    [ -z "${AI_MEMORY_TEST_UNINSTALL_FAIL:-}" ] || exit 3
    ;;
esac
FAKE
  cat >"${token_native}" <<'FAKE'
#!/usr/bin/env bash
{
  printf 'argv=%s\n' "$*"
  printf 'file=%s\n' "${AI_MEMORY_AUTH__BEARER_TOKEN_FILE:-}"
  printf 'data_dir=%s\n' "${AI_MEMORY_DATA_DIR:-}"
  if [ -n "${AI_MEMORY_AUTH__BEARER_TOKEN_FILE:-}" ]; then
    printf 'content=%s\n' "$(cat "${AI_MEMORY_AUTH__BEARER_TOKEN_FILE}")"
    printf 'mode=%s\n' "$(stat -c '%a' "${AI_MEMORY_AUTH__BEARER_TOKEN_FILE}" 2>/dev/null || stat -f '%Lp' "${AI_MEMORY_AUTH__BEARER_TOKEN_FILE}")"
  fi
} >>"${AI_MEMORY_WRAPPER_TEST_LOG}"
FAKE
  chmod 0755 "${token_docker}" "${token_native}"
  for cmd in "install-hooks --agent claude-code --apply" "run codex" \
    "mcp-bridge --server-url http://127.0.0.1:49374/mcp"; do
    : >"${token_log}"
    case "${cmd}" in install-hooks*) ;; *) : >"${token_docker_log}" ;; esac
    # shellcheck disable=SC2086 # the command string is split on purpose
    HOME="${token_home}" XDG_DATA_HOME="${token_home}/.local/share" \
      AI_MEMORY_DOCKER="${token_docker}" AI_MEMORY_NATIVE_BIN="${token_native}" \
      AI_MEMORY_DOCKER_TEST_LOG="${token_docker_log}" AI_MEMORY_WRAPPER_TEST_LOG="${token_log}" \
      bin/ai-memory ${cmd}
    assert_contains "${token_log}" "file=${token_home}/.local/share/ai-memory/native-runner/server-token"
    assert_contains "${token_log}" "content=VOLUME-ROOT-TOKEN"
    assert_contains "${token_log}" "mode=600"
    if grep -F 'argv=' "${token_log}" | grep -Fq 'VOLUME-ROOT-TOKEN'; then
      fail "the volume token reached the native client's argv for: ${cmd}"
    fi
    # Only the setup step asks the container; the managed host commands and
    # the per-session bridge read the copy it left.
    case "${cmd}" in
      install-hooks*) ;;
      *) [ ! -s "${token_docker_log}" ] || fail "${cmd} reached the container engine" ;;
    esac
  done
  : >"${token_docker_log}"
  HOME="${token_home}" XDG_DATA_HOME="${token_home}/.local/share" \
    AI_MEMORY_DOCKER="${token_docker}" AI_MEMORY_NATIVE_BIN="${token_native}" \
    AI_MEMORY_DOCKER_TEST_LOG="${token_docker_log}" AI_MEMORY_WRAPPER_TEST_LOG="${token_log}" \
    bin/ai-memory install-hooks --agent claude-code --apply
  assert_contains "${token_docker_log}" "-v ai-memory-data:/data:ro"

  # Control: a bind-mounted data dir is read directly by the native client;
  # the wrapper neither asks the container nor sets a token file.
  : >"${token_log}"
  : >"${token_docker_log}"
  HOME="${token_home}" XDG_DATA_HOME="${token_home}/.local/share" \
    AI_MEMORY_DATA_DIR="${bind_dir}" \
    AI_MEMORY_DOCKER="${token_docker}" AI_MEMORY_NATIVE_BIN="${token_native}" \
    AI_MEMORY_DOCKER_TEST_LOG="${token_docker_log}" AI_MEMORY_WRAPPER_TEST_LOG="${token_log}" \
    bin/ai-memory install-hooks --agent claude-code --apply
  assert_contains "${token_log}" "data_dir=${bind_dir}"
  assert_contains "${token_log}" "file="
  if grep -Fq 'file=/' "${token_log}" || [ -s "${token_docker_log}" ]; then
    fail "a bind-mounted data dir must not go through the volume token copy"
  fi

  # Control: an explicit AI_MEMORY_AUTH_TOKEN wins; no copy is made.
  : >"${token_log}"
  : >"${token_docker_log}"
  HOME="${token_home}" XDG_DATA_HOME="${token_home}/.local/share" \
    AI_MEMORY_AUTH_TOKEN="explicit" \
    AI_MEMORY_DOCKER="${token_docker}" AI_MEMORY_NATIVE_BIN="${token_native}" \
    AI_MEMORY_DOCKER_TEST_LOG="${token_docker_log}" AI_MEMORY_WRAPPER_TEST_LOG="${token_log}" \
    bin/ai-memory install-hooks --agent claude-code --apply
  if grep -Fq 'file=/' "${token_log}" || [ -s "${token_docker_log}" ]; then
    fail "an explicit AI_MEMORY_AUTH_TOKEN must win over the volume token copy"
  fi

  log "Checking the host token copy goes with the volume's token and with uninstall"
  local token_copy="${token_home}/.local/share/ai-memory/native-runner/server-token"
  token_wrapper() {
    HOME="${token_home}" XDG_DATA_HOME="${token_home}/.local/share" \
      AI_MEMORY_NO_VERSION_CHECK=1 AI_MEMORY_NO_TTY=1 \
      AI_MEMORY_DOCKER="${token_docker}" AI_MEMORY_NATIVE_BIN="${token_native}" \
      AI_MEMORY_DOCKER_TEST_LOG="${token_docker_log}" AI_MEMORY_WRAPPER_TEST_LOG="${token_log}" \
      bin/ai-memory "$@" </dev/null >/dev/null
  }
  token_wrapper install-hooks --agent claude-code --apply
  test -s "${token_copy}" || fail "install-hooks must leave the host token copy"
  # An engine that cannot answer keeps the copy and still hands it over.
  : >"${token_log}"
  AI_MEMORY_TEST_ENGINE_DOWN=1 token_wrapper install-hooks --agent claude-code --apply
  test -s "${token_copy}" || fail "an unreachable engine must not remove the host token copy"
  assert_contains "${token_log}" "file=${token_copy}"
  # A volume with no token any more (auth off or rotated away) removes it.
  : >"${token_log}"
  AI_MEMORY_TEST_VOLUME_TOKEN="" token_wrapper install-hooks --agent claude-code --apply
  test ! -e "${token_copy}" || fail "a volume without a token must remove the host token copy"
  if grep -Fq 'file=/' "${token_log}"; then
    fail "a removed host token copy must not be handed to the native client"
  fi
  # uninstall: a dry run, an --only subset and a failed run keep the copy; a
  # successful full --apply removes it.
  token_wrapper install-hooks --agent claude-code --apply
  token_wrapper uninstall
  test -s "${token_copy}" || fail "an uninstall dry run must keep the host token copy"
  token_wrapper uninstall --apply --only hooks
  test -s "${token_copy}" || fail "uninstall --only must keep the host token copy"
  if AI_MEMORY_TEST_UNINSTALL_FAIL=1 token_wrapper uninstall --apply; then
    fail "a failing uninstall must keep its exit status"
  fi
  test -s "${token_copy}" || fail "a failed uninstall must keep the host token copy"
  # On a terminal without --yes, uninstall asks first, and a declined prompt
  # still exits 0 with nothing removed, so the copy must stay. `script` gives
  # the wrapper a pseudo-terminal on stdin (util-linux syntax; skipped where
  # it is missing).
  if script -qec true /dev/null >/dev/null 2>&1; then
    script -qec "env HOME='${token_home}' XDG_DATA_HOME='${token_home}/.local/share' \
      AI_MEMORY_NO_VERSION_CHECK=1 AI_MEMORY_NO_TTY=1 \
      AI_MEMORY_DOCKER='${token_docker}' AI_MEMORY_NATIVE_BIN='${token_native}' \
      AI_MEMORY_DOCKER_TEST_LOG='${token_docker_log}' AI_MEMORY_WRAPPER_TEST_LOG='${token_log}' \
      bin/ai-memory uninstall --apply" /dev/null </dev/null >/dev/null 2>&1 || true
    test -s "${token_copy}" || fail "an interactive uninstall without --yes must keep the host token copy"
  fi
  token_wrapper uninstall --apply --yes
  test ! -e "${token_copy}" || fail "uninstall --apply must remove the host token copy"

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
