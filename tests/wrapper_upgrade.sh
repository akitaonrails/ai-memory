#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ai-memory-wrapper-upgrade.XXXXXX")"
trap 'rm -rf "${TMP_ROOT}"' EXIT

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

assert_contains() {
  local file="$1" needle="$2"
  grep -Fq -- "${needle}" "${file}" \
    || fail "${file} does not contain expected text: ${needle}"
}

assert_not_contains() {
  local file="$1" needle="$2"
  if grep -Fq -- "${needle}" "${file}"; then
    fail "${file} unexpectedly contains: ${needle}"
  fi
}

FAKE_DOCKER="${TMP_ROOT}/podman"
cat >"${FAKE_DOCKER}" <<'DOCKER'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${AI_MEMORY_WRAPPER_TEST_LOG}"

case "${1:-}" in
  pull)
    exit 0
    ;;
  ps)
    printf 'ai-memory\n'
    ;;
  compose)
    if [ "${2:-}" = "ps" ] && [ "${AI_MEMORY_TEST_COMPOSE_OWNS:-}" = "1" ]; then
      printf 'running-container-id\n'
    fi
    ;;
  inspect)
    case "${4:-}" in
      '{{.Id}}') printf 'running-container-id\n' ;;
      '{{.Config.Image}}') printf 'akitaonrails/ai-memory:latest\n' ;;
      *PortBindings*) printf '%s\n' '-p 127.0.0.1:49374:49374/tcp ' ;;
      *Mounts*) printf '%s\n' '-v ai-memory-data:/data ' ;;
      *RestartPolicy*) printf '%s\n' '--restart unless-stopped' ;;
      '{{json .Config.Cmd}}') printf '[]\n' ;;
      *'.Config.Env'*) : ;;
      *) printf 'unexpected inspect format: %s\n' "${4:-<missing>}" >&2; exit 2 ;;
    esac
    ;;
  *)
    printf 'unexpected docker command: %s\n' "$*" >&2
    exit 2
    ;;
esac
DOCKER
chmod 0755 "${FAKE_DOCKER}"

run_upgrade_case() {
  local name="$1" owns="$2" case_dir log output
  case_dir="${TMP_ROOT}/${name}"
  log="${case_dir}/docker.log"
  output="${case_dir}/output.log"
  mkdir -p "${case_dir}/home" "${case_dir}/cache"
  : >"${case_dir}/docker-compose.yml"

  (
    cd "${case_dir}"
    HOME="${case_dir}/home" \
    XDG_CACHE_HOME="${case_dir}/cache" \
    AI_MEMORY_DOCKER="${FAKE_DOCKER}" \
    AI_MEMORY_SKIP_SELF_UPGRADE=1 \
    AI_MEMORY_WRAPPER_TEST_LOG="${log}" \
    AI_MEMORY_TEST_COMPOSE_OWNS="${owns}" \
      "${ROOT}/bin/ai-memory" upgrade >"${output}" 2>&1
  )
}

run_upgrade_case standalone 0
assert_contains "${TMP_ROOT}/standalone/output.log" "does not manage the running ai-memory container"
assert_not_contains "${TMP_ROOT}/standalone/docker.log" "compose up -d"
assert_contains "${TMP_ROOT}/standalone/cache/ai-memory/recreate-ai-memory.sh" "-v ai-memory-data:/data"
assert_contains "${TMP_ROOT}/standalone/cache/ai-memory/recreate-ai-memory.sh" "${FAKE_DOCKER} stop ai-memory"
assert_not_contains "${TMP_ROOT}/standalone/cache/ai-memory/recreate-ai-memory.sh" "docker stop ai-memory"

run_upgrade_case compose 1
assert_contains "${TMP_ROOT}/compose/output.log" "restarting local ai-memory container via ${FAKE_DOCKER} compose"
assert_contains "${TMP_ROOT}/compose/docker.log" "compose up -d"
if [ -e "${TMP_ROOT}/compose/cache/ai-memory/recreate-ai-memory.sh" ]; then
  fail "Compose-owned container unexpectedly produced a standalone recreation script"
fi

printf 'wrapper upgrade ownership checks passed\n'
