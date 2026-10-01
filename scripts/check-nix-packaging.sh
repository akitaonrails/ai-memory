#!/usr/bin/env bash
# CI-safe checks for the Nix flake package output.
#
# Validates the built prefix (hooks, config template, binary execution) and
# `nix run` wiring. Does not mutate the host or require systemd.

set -euo pipefail

PREFIX="${1:-}"

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

main() {
  if [ -z "${PREFIX}" ]; then
    fail "usage: $0 <nix-build-result-prefix>"
  fi

  local binary="${PREFIX}/bin/ai-memory"
  if [ ! -x "${binary}" ]; then
    fail "missing executable: ${binary}"
  fi

  log "Checking ai-memory --version"
  "${binary}" --version

  local hooks_dir="${PREFIX}/share/ai-memory/hooks"
  if [ ! -d "${hooks_dir}" ]; then
    fail "missing hooks directory: ${hooks_dir}"
  fi
  if [ -z "$(ls -A "${hooks_dir}" 2>/dev/null)" ]; then
    fail "hooks directory is empty: ${hooks_dir}"
  fi

  local config_template="${PREFIX}/etc/ai-memory/config.default.toml"
  if [ ! -f "${config_template}" ]; then
    fail "missing config template: ${config_template}"
  fi

  log "Checking nix run smoke"
  cd "$(repo_root)"
  nix run . -- --version

  log "Nix packaging checks passed"
}

main "$@"
