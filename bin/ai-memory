#!/usr/bin/env bash
# ai-memory — thin wrapper that invokes the dockerised ai-memory
# binary with the right mounts so install-* commands can edit your
# real config files, bootstrap can read your real project, and
# data-dir commands hit the same volume your server uses.
#
# Install this to ~/.local/bin/ai-memory (ensure ~/.local/bin is on
# PATH). See README "Quick start" for the one-liner.
#
# Special wrapper-only subcommands (not forwarded to the binary):
#   ai-memory upgrade       Pull the latest image + remind to re-stage hooks.
#   ai-memory run ...       Use a cached native client so it can exec host agents.
#   ai-memory show ...      Use that client for host project/harness discovery.
#   ai-memory continue ...  Use that client to resume the newest linked checkout.
#   ai-memory workstreams   Use that client to inspect the host checkout identity.
#
# Env overrides:
#   AI_MEMORY_IMAGE         docker image (default: akitaonrails/ai-memory:latest)
#   AI_MEMORY_DOCKER        docker command (default: docker; e.g. "podman")
#   AI_MEMORY_DATA_VOLUME   named volume mounted at /data (default: ai-memory-data)
#   AI_MEMORY_DATA_DIR      host path to bind-mount at /data instead (rare)
#   AI_MEMORY_SERVER_URL    server URL for thin-client commands; if unset,
#                           the wrapper reaches the host loopback server
#                           started by the README quick start.
#   COPILOT_GITHUB_TOKEN    GitHub token for AI_MEMORY_LLM_PROVIDER=copilot
#   ANTHROPIC_OAUTH_TOKEN   Claude subscription token forwarded to helper
#                           commands
#   CLAUDE_CODE_OAUTH_TOKEN fallback Claude subscription token forwarded
#                           likewise
#   AI_MEMORY_NO_TTY=1      force non-interactive even on a real tty
#   AI_MEMORY_NO_VERSION_CHECK=1  skip the once-per-day update check
#   AI_MEMORY_NATIVE_BIN    native binary used for managed host commands (optional)
#   AI_MEMORY_WRAPPER_URL   wrapper release asset override (optional; its
#                           .sha256 companion is required)
#   AI_MEMORY_WRAPPER_SHA256_URL  wrapper checksum URL override (optional)
#   CLAUDE_CONFIG_DIR       Claude Code config root (forwarded to the helper;
#                           paths under $HOME are covered by the home bind mount)
set -euo pipefail

IMAGE="${AI_MEMORY_IMAGE:-akitaonrails/ai-memory:latest}"
DOCKER="${AI_MEMORY_DOCKER:-docker}"
DATA_VOLUME="${AI_MEMORY_DATA_VOLUME:-ai-memory-data}"
CACHE_DIR="${XDG_CACHE_HOME:-${HOME}/.cache}/ai-memory"
VERSION_CHECK_FILE="${CACHE_DIR}/last-version-check"
HOOKS_STAGE_DIR="${HOME}/.local/share/ai-memory/hooks"
WRAPPER_URL="${AI_MEMORY_WRAPPER_URL:-https://github.com/akitaonrails/ai-memory/releases/latest/download/ai-memory-wrapper}"
WRAPPER_SHA256_URL="${AI_MEMORY_WRAPPER_SHA256_URL:-${WRAPPER_URL}.sha256}"

# ---- version-check helpers (best-effort; never block the wrapper) --------

local_digest() {
  "${DOCKER}" image inspect --format='{{index .RepoDigests 0}}' "${IMAGE}" 2>/dev/null \
    | sed 's/.*@//' || true
}

remote_digest() {
  # docker manifest inspect was experimental pre-20.10; on modern
  # docker it's stable. Silent on failure (offline, podman, etc.).
  "${DOCKER}" manifest inspect "${IMAGE}" 2>/dev/null \
    | grep -oE 'sha256:[a-f0-9]{64}' | head -n 1 || true
}

maybe_warn_outdated() {
  [ -z "${AI_MEMORY_NO_VERSION_CHECK:-}" ] || return 0
  # Skip in non-interactive contexts so we don't pollute pipes / CI.
  [ -t 2 ] || return 0
  # Skip if we checked within the last 24h (works on both BSD + GNU find).
  if [ -f "${VERSION_CHECK_FILE}" ] \
     && [ -z "$(find "${VERSION_CHECK_FILE}" -mtime +0 2>/dev/null)" ]; then
    return 0
  fi
  mkdir -p "${CACHE_DIR}"
  touch "${VERSION_CHECK_FILE}"
  local local_d remote_d
  local_d=$(local_digest)
  remote_d=$(remote_digest)
  [ -n "${local_d}" ] && [ -n "${remote_d}" ] || return 0
  if [ "${local_d}" != "${remote_d}" ]; then
    printf '\033[33mai-memory: a newer image is available on Docker Hub.\033[0m\n' >&2
    printf '           run `ai-memory upgrade` to pull it + refresh hooks.\n' >&2
  fi
}

# ---- upgrade subcommand --------------------------------------------------

self_upgrade_script() {
  command -v curl >/dev/null 2>&1 || { echo "  curl not found; skipping wrapper self-upgrade" >&2; return 0; }
  # Portable script-path resolution (works without GNU readlink/realpath).
  local script_dir script_path tmp checksum_file expected_sum actual_sum
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  script_path="${script_dir}/$(basename "${BASH_SOURCE[0]}")"
  echo "→ checking for wrapper script updates (${script_path})"
  tmp="$(mktemp "${script_dir}/.ai-memory-wrapper.XXXXXX")" || return 0
  checksum_file="$(mktemp)" || { rm -f "${tmp}"; return 0; }
  if ! curl -fsSL "${WRAPPER_URL}" -o "${tmp}" 2>/dev/null; then
    echo "  could not fetch ${WRAPPER_URL}; skipping self-upgrade"
    rm -f "${tmp}" "${checksum_file}"
    return 0
  fi
  if ! curl -fsSL "${WRAPPER_SHA256_URL}" -o "${checksum_file}" 2>/dev/null; then
    echo "  could not fetch ${WRAPPER_SHA256_URL}; refusing an unverified wrapper update"
    rm -f "${tmp}" "${checksum_file}"
    return 0
  fi
  expected_sum="$(awk 'NR == 1 && $1 ~ /^[0-9A-Fa-f]{64}$/ { print tolower($1) }' "${checksum_file}")"
  if command -v sha256sum >/dev/null 2>&1; then
    actual_sum="$(sha256sum "${tmp}" | awk '{ print $1 }')"
  elif command -v shasum >/dev/null 2>&1; then
    actual_sum="$(shasum -a 256 "${tmp}" | awk '{ print $1 }')"
  else
    echo "  sha256sum/shasum not found; refusing an unverified wrapper update"
    rm -f "${tmp}" "${checksum_file}"
    return 0
  fi
  rm -f "${checksum_file}"
  if [ -z "${expected_sum}" ] || [ "${actual_sum}" != "${expected_sum}" ]; then
    echo "  wrapper checksum mismatch; refusing update"
    rm -f "${tmp}"
    return 0
  fi
  # Sanity: must look like our bash script. Refuse to install
  # anything that doesn't start with the expected shebang.
  if ! head -n1 "${tmp}" | grep -q '^#!/usr/bin/env bash'; then
    echo "  downloaded file doesn't look like the ai-memory wrapper; skipping"
    rm -f "${tmp}"
    return 0
  fi
  if cmp -s "${tmp}" "${script_path}"; then
    echo "  wrapper already up to date"
    rm -f "${tmp}"
    return 0
  fi
  chmod +x "${tmp}"
  if ! mv "${tmp}" "${script_path}" 2>/dev/null; then
    echo "  could not replace ${script_path} (permission denied?)"
    echo "  rerun with: sudo install -m 0755 ${tmp} ${script_path}"
    return 0
  fi
  echo "  ✓ wrapper updated — re-executing with the new version"
  AI_MEMORY_SKIP_SELF_UPGRADE=1 exec "${script_path}" upgrade
}


# Reconstruct the `docker run` that created a running container, so a
# non-compose install can be recreated on the freshly pulled image without
# the operator having to remember their original flags (issue #407).
#
# Written to a 0600 file rather than echoed: the environment of a real
# install carries provider API keys and AI_MEMORY_AUTH_TOKEN, and printing
# those into terminal scrollback — or into a piped install log — is exactly
# the kind of leak this project exists to prevent.
#
# Reconstructs the flags that matter for an ai-memory container: name,
# restart policy, published ports, mounts, operator-supplied environment,
# and an overridden command. It does NOT reproduce every possible `docker
# run` flag (networks, capabilities, resource limits, devices); the script
# says so in its own header so a reader can add anything exotic back.
emit_docker_run_script() {
  local container="$1" out="$2" image
  image="$("${DOCKER}" inspect "${container}" --format '{{.Config.Image}}' 2>/dev/null)" || return 1
  [ -n "${image}" ] || return 1

  local ports volumes restart cmd
  ports="$("${DOCKER}" inspect "${container}" --format \
    '{{range $p, $bindings := .HostConfig.PortBindings}}{{range $bindings}}-p {{if .HostIp}}{{.HostIp}}:{{end}}{{.HostPort}}:{{$p}} {{end}}{{end}}' 2>/dev/null)"
  volumes="$("${DOCKER}" inspect "${container}" --format \
    '{{range .Mounts}}{{if eq .Type "volume"}}-v {{.Name}}:{{.Destination}} {{else}}-v {{.Source}}:{{.Destination}} {{end}}{{end}}' 2>/dev/null)"
  restart="$("${DOCKER}" inspect "${container}" --format \
    '{{with .HostConfig.RestartPolicy.Name}}{{if ne . "no"}}--restart {{.}}{{end}}{{end}}' 2>/dev/null)"

  # Only the command if the operator overrode the image default; otherwise
  # let the new image supply its own (that is the point of upgrading).
  local image_cmd container_cmd
  image_cmd="$("${DOCKER}" inspect "${image}" --format '{{json .Config.Cmd}}' 2>/dev/null)"
  container_cmd="$("${DOCKER}" inspect "${container}" --format '{{json .Config.Cmd}}' 2>/dev/null)"
  cmd=""
  if [ "${image_cmd}" != "${container_cmd}" ]; then
    cmd="$("${DOCKER}" inspect "${container}" --format \
      '{{range .Config.Cmd}}{{printf "%q " .}}{{end}}' 2>/dev/null)"
  fi

  # Likewise, carry only environment the operator actually passed. Anything
  # baked into the image is re-applied by the new image anyway, and pinning
  # it here would freeze a value this upgrade is meant to move forward.
  local image_env container_env env_flags=""
  image_env="$("${DOCKER}" inspect "${image}" --format '{{range .Config.Env}}{{println .}}{{end}}' 2>/dev/null)"
  container_env="$("${DOCKER}" inspect "${container}" --format '{{range .Config.Env}}{{println .}}{{end}}' 2>/dev/null)"
  local kv
  while IFS= read -r kv; do
    [ -n "${kv}" ] || continue
    if printf '%s\n' "${image_env}" | grep -qxF -- "${kv}"; then
      continue
    fi
    env_flags="${env_flags} -e $(printf '%q' "${kv}")"
  done <<ENVEOF
${container_env}
ENVEOF

  ( umask 077 && : > "${out}" ) || return 1
  {
    echo "#!/usr/bin/env bash"
    echo "# Reconstructed by \`ai-memory upgrade\` from the running '${container}'"
    echo "# container, before it was removed. Review it, then run it."
    echo "#"
    echo "# Contains this install's environment (API keys, auth token) — it is"
    echo "# mode 0600 for that reason. Delete it once the container is back up."
    echo "#"
    echo "# Covers name, restart policy, ports, mounts, operator-set environment"
    echo "# and an overridden command. If your original command used anything"
    echo "# else (custom network, capabilities, resource limits), re-add it."
    echo "set -euo pipefail"
    echo
    echo "docker stop ${container}"
    echo "docker rm ${container}"
    printf 'docker run -d --name %s' "${container}"
    [ -n "${restart}" ] && printf ' %s' "${restart}"
    [ -n "${ports}" ] && printf ' %s' "${ports% }"
    [ -n "${volumes}" ] && printf ' %s' "${volumes% }"
    [ -n "${env_flags}" ] && printf '%s' "${env_flags}"
    printf ' %s' "${image}"
    [ -n "${cmd}" ] && printf ' %s' "${cmd% }"
    printf '\n'
  } >> "${out}"
  chmod 600 "${out}" 2>/dev/null || true
  return 0
}

# A compose file found in a conventional location is only relevant when its
# project actually owns the running container. A stale or unrelated file may
# declare the same container_name and `compose up` would then fail with a name
# conflict instead of taking the safe standalone recovery path.
compose_manages_container() {
  local container="$1" compose_dir="$2" container_id compose_id
  container_id="$("${DOCKER}" inspect "${container}" --format '{{.Id}}' 2>/dev/null)" || return 1
  [ -n "${container_id}" ] || return 1

  while IFS= read -r compose_id; do
    [ -n "${compose_id}" ] || continue
    case "${container_id}" in
      "${compose_id}"*) return 0 ;;
    esac
    case "${compose_id}" in
      "${container_id}"*) return 0 ;;
    esac
  done < <((cd "${compose_dir}" && "${DOCKER}" compose ps -q) 2>/dev/null)
  return 1
}

cmd_upgrade() {
  if [ -z "${AI_MEMORY_SKIP_SELF_UPGRADE:-}" ]; then
    self_upgrade_script
  fi
  echo "→ pulling ${IMAGE}"
  "${DOCKER}" pull "${IMAGE}"
  rm -f "${CACHE_DIR}/native-runner/last-check"

  local found_agents=()
  if [ -d "${HOOKS_STAGE_DIR}" ]; then
    for d in "${HOOKS_STAGE_DIR}"/*/; do
      [ -d "${d}" ] || continue
      name="$(basename "${d}")"
      # `lib` (and any `_`-prefixed dir) holds shared hook helpers sourced
      # by the per-agent scripts — it is NOT an agent. Skip it so we don't
      # run `install-hooks --agent lib`, which clap rejects. (issue #38)
      case "${name}" in
        lib | _*) continue ;;
      esac
      found_agents+=("${name}")
    done
  fi

  echo
  if [ "${#found_agents[@]}" -gt 0 ]; then
    echo "→ refreshing staged hook scripts for: ${found_agents[*]}"
    for agent in "${found_agents[@]}"; do
      # `install-hooks --apply` re-stages scripts AND idempotently
      # rewrites the agent's settings.json entry. Safe to re-run:
      # the seven hook keys we own get replaced; everything else
      # (other hooks the user wired up) survives untouched.
      echo "    ai-memory install-hooks --agent ${agent} --apply"
      echo "      (uses AI_MEMORY_SERVER_URL/AI_MEMORY_AUTH_TOKEN or the existing ai-memory MCP entry when present)"
      AI_MEMORY_NO_VERSION_CHECK=1 \
      "$0" install-hooks --agent "${agent}" --apply \
        || echo "      (skipped — re-run with the same --server-url / --auth-token used originally)"
    done
  else
    echo "→ no staged hook scripts found at ${HOOKS_STAGE_DIR}"
    echo "  (nothing to refresh — install-hooks hasn't been run with --apply yet)"
  fi

  echo
  if "${DOCKER}" ps --filter "name=^ai-memory$" --format '{{.Names}}' 2>/dev/null \
       | grep -q '^ai-memory$'; then
    # A LOCAL ai-memory container is running. We've just pulled a new
    # image; `docker restart` won't recreate from the new image, so we
    # need to stop+remove+recreate. Use `docker compose up -d` only when
    # the discovered project owns this container; otherwise reconstruct
    # the standalone `docker run` from its inspected runtime settings.
    local compose_dir=""
    if [ -f "$(pwd)/docker/docker-compose.yml" ]; then
      compose_dir="$(pwd)/docker"
    elif [ -f "$(pwd)/docker-compose.yml" ]; then
      compose_dir="$(pwd)"
    elif [ -f "${HOME}/deploy/ai-memory/docker-compose.yml" ]; then
      compose_dir="${HOME}/deploy/ai-memory"
    fi
    if [ -n "${compose_dir}" ] && compose_manages_container "ai-memory" "${compose_dir}"; then
      echo "→ restarting local ai-memory container via docker compose (${compose_dir})"
      ( cd "${compose_dir}" && "${DOCKER}" compose up -d ) \
        || echo "  (compose restart failed; re-run manually: cd ${compose_dir} && docker compose up -d)"
    else
      if [ -n "${compose_dir}" ]; then
        echo "→ the compose file at ${compose_dir} does not manage the running ai-memory container"
        echo "  Treating it as a standalone docker run install to avoid a container-name conflict."
      else
        echo "→ a local ai-memory container is running but no compose file"
        echo "  was found in \$PWD/docker-compose.yml, ./docker/docker-compose.yml,"
        echo "  or ~/deploy/ai-memory/docker-compose.yml."
      fi
      local recreate_script="${CACHE_DIR}/recreate-ai-memory.sh"
      mkdir -p "${CACHE_DIR}" 2>/dev/null || true
      if emit_docker_run_script "ai-memory" "${recreate_script}"; then
        echo "  Wrote the equivalent stop/remove/run for THIS container to:"
        echo "      ${recreate_script}"
        echo "  Review it and run it so the new image takes effect:"
        echo "      less ${recreate_script} && bash ${recreate_script}"
        echo "  (mode 0600 — it carries the environment of this install, including"
        echo "  any API keys and AI_MEMORY_AUTH_TOKEN. Delete it once you are up.)"
      else
        echo "  Could not inspect the running container to reconstruct its"
        echo "  command. Restart it manually so the new image takes effect:"
        echo "      docker stop ai-memory && docker rm ai-memory"
        echo "      # then re-run your docker-run command from the README Quick start"
      fi
    fi
  fi

  # If the server runs on a different host (homelab scenario), this command
  # cannot know or change that host's deployment state.
  if [ -n "${AI_MEMORY_SERVER_URL:-}" ] \
     && ! echo "${AI_MEMORY_SERVER_URL}" | grep -qE '^https?://(127\.|localhost|\[?::1\]?)'; then
    echo
    echo "→ Note: AI_MEMORY_SERVER_URL points at ${AI_MEMORY_SERVER_URL}"
    echo "  ai-memory upgrade updates this wrapper and local image only; it does not"
    echo "  inspect or redeploy the remote server. If that host is not already current,"
    echo "  run \`bin/deploy\` or \`docker compose pull && docker compose up -d\`"
    echo "  in its deploy directory."
  fi

  mkdir -p "${CACHE_DIR}"
  touch "${VERSION_CHECK_FILE}"
}

# Managed launch/discovery commands must execute on the host: checkouts, harnesses,
# and native transcript stores are host resources, not contents of the helper
# container. Keep a checksum-verified release client beside the wrapper cache.
native_host_binary() {
  if [ -n "${AI_MEMORY_NATIVE_BIN:-}" ]; then
    [ -x "${AI_MEMORY_NATIVE_BIN}" ] || {
      echo "ai-memory: AI_MEMORY_NATIVE_BIN is not executable: ${AI_MEMORY_NATIVE_BIN}" >&2
      return 1
    }
    printf '%s\n' "${AI_MEMORY_NATIVE_BIN}"
    return 0
  fi

  local os arch artifact base native_dir binary archive check_file need_refresh remote_sum local_sum tmp
  case "$(uname -s 2>/dev/null || true)" in
    Linux) os="linux" ;;
    Darwin) os="macos" ;;
    *)
      echo "ai-memory: the Docker wrapper cannot provide managed host launches on this OS" >&2
      echo "install the native release binary to use ai-memory run/show/continue/workstreams" >&2
      return 1
      ;;
  esac
  case "$(uname -m 2>/dev/null || true)" in
    x86_64 | amd64) arch="x86_64" ;;
    aarch64 | arm64) arch="aarch64" ;;
    *)
      echo "ai-memory: no host-launch release binary is published for $(uname -m)" >&2
      return 1
      ;;
  esac
  artifact="ai-memory-${os}-${arch}"
  base="https://github.com/akitaonrails/ai-memory/releases/latest/download/${artifact}.tar.gz"
  native_dir="${CACHE_DIR}/native-runner"
  binary="${native_dir}/ai-memory"
  archive="${native_dir}/${artifact}.tar.gz"
  check_file="${native_dir}/last-check"
  mkdir -p "${native_dir}"

  need_refresh=0
  [ -x "${binary}" ] && [ -f "${archive}" ] || need_refresh=1
  if [ "${need_refresh}" -eq 0 ] && { [ ! -f "${check_file}" ] || [ -n "$(find "${check_file}" -mtime +0 2>/dev/null)" ]; }; then
    remote_sum=$(curl -fsSL "${base}.sha256" 2>/dev/null | awk 'NR == 1 { print $1 }' || true)
    if command -v sha256sum >/dev/null 2>&1; then
      local_sum=$(sha256sum "${archive}" | awk '{ print $1 }')
    else
      local_sum=$(shasum -a 256 "${archive}" | awk '{ print $1 }')
    fi
    if [ -n "${remote_sum}" ] && [ "${remote_sum}" != "${local_sum}" ]; then
      need_refresh=1
    fi
    touch "${check_file}"
  fi

  if [ "${need_refresh}" -eq 1 ]; then
    command -v curl >/dev/null 2>&1 || {
      echo "ai-memory: curl is required to install the native host-launch client" >&2
      return 1
    }
    tmp="${native_dir}/.install-$$"
    rm -rf "${tmp}"
    mkdir -p "${tmp}"
    echo "ai-memory: installing checksum-verified native host-launch client (${artifact})" >&2
    curl -fsSL "${base}" -o "${tmp}/${artifact}.tar.gz"
    curl -fsSL "${base}.sha256" -o "${tmp}/${artifact}.tar.gz.sha256"
    if command -v sha256sum >/dev/null 2>&1; then
      (cd "${tmp}" && sha256sum -c "${artifact}.tar.gz.sha256" >/dev/null)
    else
      remote_sum=$(awk 'NR == 1 { print $1 }' "${tmp}/${artifact}.tar.gz.sha256")
      local_sum=$(shasum -a 256 "${tmp}/${artifact}.tar.gz" | awk '{ print $1 }')
      [ "${remote_sum}" = "${local_sum}" ] || {
        echo "ai-memory: native host-launch client checksum mismatch" >&2
        rm -rf "${tmp}"
        return 1
      }
    fi
    tar -xzf "${tmp}/${artifact}.tar.gz" -C "${tmp}"
    [ -x "${tmp}/ai-memory" ] || chmod +x "${tmp}/ai-memory"
    mv "${tmp}/${artifact}.tar.gz" "${archive}"
    mv "${tmp}/ai-memory" "${binary}"
    rm -rf "${tmp}"
    touch "${check_file}"
  fi
  printf '%s\n' "${binary}"
}

# ---- intercept wrapper-only subcommands ----------------------------------

case "${1:-}" in
  upgrade)
    shift
    cmd_upgrade
    exit 0
    ;;
  run | show | continue | workstreams)
    NATIVE_HOST_COMMAND=$1
    shift
    NATIVE_HOST_BIN=$(native_host_binary)
    exec "${NATIVE_HOST_BIN}" "${NATIVE_HOST_COMMAND}" "$@"
    ;;
esac

# ---- normal pass-through to the binary inside docker ---------------------

maybe_warn_outdated || true

# Keep stdin attached in every mode. This covers pipes and redirects, plus the
# easy-to-miss terminal-stdin/redirected-stdout case. `AI_MEMORY_NO_TTY`
# suppresses only the pseudo-terminal allocation.
TTY_ARGS=(-i)
if [ -z "${AI_MEMORY_NO_TTY:-}" ] && [ -t 0 ] && [ -t 1 ]; then
  TTY_ARGS+=(-t)
fi

# The CLI runs INSIDE the container but renders hook config for the HOST, which
# has no local ai-memory binary. Native installs default to `posix-native` (the
# binary hook command); the wrapper must NOT inherit that or it would bake the
# *container's* binary path into the host's hook config. Force the shell-script
# platform (`posix`) unless the operator chose one explicitly.
export AI_MEMORY_HOOK_PLATFORM="${AI_MEMORY_HOOK_PLATFORM:-posix}"

ENV_ARGS=()
for var in \
  AI_MEMORY_SERVER_URL \
  AI_MEMORY_AUTH_TOKEN \
  AI_MEMORY_LLM_PROVIDER \
  AI_MEMORY_LLM_MODEL \
  AI_MEMORY_LLM_BASE_URL \
  AI_MEMORY_COPILOT_CLIENT_ID \
  AI_MEMORY_EMBEDDING_PROVIDER \
  AI_MEMORY_EMBEDDING_MODEL \
  AI_MEMORY_EMBEDDING_BASE_URL \
  AI_MEMORY_EMBEDDING_DIM \
  AI_MEMORY_ALLOWED_HOSTS \
  AI_MEMORY_WORKSTREAM_ID \
  AI_MEMORY_HOOK_PLATFORM \
  AI_MEMORY_HOOKS_HOST_ROOT \
  CLAUDE_CONFIG_DIR \
  CLAUDE_CODE_SESSION_ID \
  ANTHROPIC_API_KEY \
  ANTHROPIC_OAUTH_TOKEN \
  CLAUDE_CODE_OAUTH_TOKEN \
  OPENAI_API_KEY \
  COPILOT_GITHUB_TOKEN \
  GITHUB_COPILOT_API_TOKEN \
  COPILOT_API_URL \
  VOYAGE_API_KEY \
  LLM_API_KEY \
  EMBEDDING_API_KEY \
  RUST_LOG
do
  if [ -n "${!var:-}" ]; then
    ENV_ARGS+=(-e "${var}")
  fi
done

# The wrapper itself runs the CLI inside a short-lived helper container, while
# the README server runs in the long-lived ai-memory container and publishes
# 127.0.0.1:49374 on the host. Inside a normal bridge-network helper,
# 127.0.0.1 would mean "this helper container", so thin-client commands like
# `status` and `bootstrap` could not reach the default server.
NETWORK_ARGS=()
# Default: map the container process to the host user so files written
# through bind mounts (~/.claude/settings.json, $PWD/, …) stay editable
# by the invoking user. macOS is one exception (see Darwin arm); rootless
# Docker is the other (see the WRITES_HOST_FILES check below).
USER_ARGS=(-u "$(id -u):$(id -g)")

# Does this invocation RENDER host-side agent config? install-mcp,
# install-hooks and setup-agent bake a server URL into files the *host*
# agent reads (the MCP `url` and the `AI_MEMORY_HOOK_URL=…` on every hook
# command). Thin-client commands (status, search, bootstrap, …) instead make
# HTTP calls FROM this helper container to the server. On macOS those two
# needs require different hosts, so the Darwin arm keys off this flag to pick
# a host-reachable URL for config vs. a container-reachable URL for HTTP.
# (issue #107)
RENDERS_HOST_CONFIG=0
WRAPPER_SUBCOMMAND=""
EXPLICIT_HOST_CONFIG=0
WRAPPER_ARGS=("$@")
idx=0
while [ "${idx}" -lt "${#WRAPPER_ARGS[@]}" ]; do
  arg="${WRAPPER_ARGS[$idx]}"
  case "${arg}" in
    --config)
      EXPLICIT_HOST_CONFIG=1
      idx=$((idx + 2))
      ;;
    --config=*)
      EXPLICIT_HOST_CONFIG=1
      idx=$((idx + 1))
      ;;
    --data-dir)
      idx=$((idx + 2))
      ;;
    --data-dir=*)
      idx=$((idx + 1))
      ;;
    --*)
      idx=$((idx + 1))
      ;;
    *)
      WRAPPER_SUBCOMMAND="${arg}"
      break
      ;;
  esac
done
WRITES_HOST_FILES=0
case "${WRAPPER_SUBCOMMAND}" in
    install-mcp | install-hooks | setup-agent)
      RENDERS_HOST_CONFIG=1
      WRITES_HOST_FILES=1
      ;;
    # uninstall edits the same host agent-config files the install-*
    # commands write; backup writes its tarball to a host path. Both go
    # through the same $HOME/$PWD bind mounts, so they need the same
    # rootless-Docker UID treatment.
    install-instructions | install-skills | uninstall | backup)
      WRITES_HOST_FILES=1
      ;;
esac
# bootstrap only *reads* host files — the repo bind-mounted at /work and the
# `.ai-memory.toml` markers under $HOME — but an unmapped UID and a confined
# SELinux label block reads exactly as hard as writes. The failure is
# two-stage and misleading: bootstrap first degrades silently to
# "no .git found at /work; bootstrapping from README/docs/rules only", then
# dies with "Permission denied (os error 13)" when it reaches a file it
# cannot skip. So readers need the same treatment as writers.
READS_HOST_FILES=0
case "${WRAPPER_SUBCOMMAND}" in
    bootstrap | restore)
      READS_HOST_FILES=1
      ;;
esac
if [ "${EXPLICIT_HOST_CONFIG}" -eq 1 ]; then
  READS_HOST_FILES=1
fi
# A custom data directory is a host bind at /data, so every command that opens
# the store touches host files even when it would otherwise be a thin client.
HOST_DATA_BIND=0
if [ -n "${AI_MEMORY_DATA_DIR:-}" ] && [ -d "${AI_MEMORY_DATA_DIR}" ]; then
  HOST_DATA_BIND=1
fi
TOUCHES_HOST_FILES=0
if [ "${WRITES_HOST_FILES}" -eq 1 ] \
  || [ "${READS_HOST_FILES}" -eq 1 ] \
  || [ "${HOST_DATA_BIND}" -eq 1 ]; then
  TOUCHES_HOST_FILES=1
fi

# Rootless Docker runs the whole daemon inside its own user namespace, where
# container UID 0 maps back to the invoking host user, but any *non-zero*
# UID we pass (e.g. our own "$(id -u):$(id -g)" above) is instead routed
# through a separate subordinate-UID range (/etc/subuid, typically 100000+)
# that has no relation to real host file ownership. So under rootless
# Docker, "-u <host-uid>:<host-gid>" writes end up owned by an unmapped
# UID and every bind-mounted write (~/.claude/settings.json, the hook
# staging dir, $PWD/CLAUDE.md, skill directories, …) fails with
# EACCES/ENOENT. Running as UID 0 instead hits rootlesskit's primary
# mapping and lands as the real host user. Only do this for commands that
# touch a host bind mount: other commands only touch the /data named
# volume, which isn't host-visible and doesn't have this problem, so leave
# their UID mapping alone.
DOCKER_SECURITY_OPTIONS=$("${DOCKER}" info --format '{{.SecurityOptions}}' 2>/dev/null || true)
ROOTLESS_DOCKER=0
CONTAINER_SELINUX=0
if printf '%s\n' "${DOCKER_SECURITY_OPTIONS}" | grep -q 'name=rootless'; then
  ROOTLESS_DOCKER=1
fi
if printf '%s\n' "${DOCKER_SECURITY_OPTIONS}" | grep -q 'name=selinux'; then
  CONTAINER_SELINUX=1
fi
# `.SecurityOptions` is a Docker-only field. Podman — including through the
# podman-docker `docker` shim — fails the template with "can't evaluate field
# SecurityOptions in type system.infoReport" and exits 125, which the `|| true`
# above swallows into an empty string. Both gates then read as "not rootless,
# no SELinux" and rootless podman runs the helper as an unmapped subordinate
# UID under a confined label, so every host-file access dies with
# "Permission denied (os error 13)". Podman exposes the same two facts under
# different keys, so ask it directly when the Docker probe came back empty.
if [ -z "${DOCKER_SECURITY_OPTIONS}" ]; then
  if [ "$("${DOCKER}" info --format '{{.Host.Security.Rootless}}' 2>/dev/null || true)" = "true" ]; then
    ROOTLESS_DOCKER=1
  fi
  if [ "$("${DOCKER}" info --format '{{.Host.Security.SELinuxEnabled}}' 2>/dev/null || true)" = "true" ]; then
    CONTAINER_SELINUX=1
  fi
fi
if [ "${TOUCHES_HOST_FILES}" -eq 1 ] && [ "${ROOTLESS_DOCKER}" -eq 1 ]; then
  USER_ARGS=(-u 0:0)
fi

SELINUX_ARGS=()
case "$(uname -s 2>/dev/null || true)" in
  Linux)
    if [ -z "${AI_MEMORY_SERVER_URL:-}" ]; then
      NETWORK_ARGS=(--network host)
    fi
    # SELinux blocks the helper's container label from writing normal home
    # labels even when its uid/gid match the host user. Relabeling the entire
    # $HOME bind with :z/:Z is unsafe, so relax label confinement only for the
    # short-lived, trusted helper commands that actually touch host files.
    SELINUX_MODE=$(getenforce 2>/dev/null || true)
    if [ -z "${SELINUX_MODE}" ] && [ -r /sys/fs/selinux/enforce ]; then
      SELINUX_MODE=$(cat /sys/fs/selinux/enforce 2>/dev/null || true)
    fi
    if [ "${TOUCHES_HOST_FILES}" -eq 1 ] \
      && { [ "${SELINUX_MODE}" = "Enforcing" ] || [ "${SELINUX_MODE}" = "enforcing" ] || [ "${SELINUX_MODE}" = "1" ]; } \
      && [ "${CONTAINER_SELINUX}" -eq 1 ]; then
      SELINUX_ARGS=(--security-opt label=disable)
    fi
    ;;
  Darwin)
    # macOS has no Docker host networking, so a thin-client command reaches
    # the host-published loopback server from this helper container via Docker
    # Desktop's host alias. But install-mcp/install-hooks/setup-agent RENDER
    # the URL into the *host* agent config, and host.docker.internal does NOT
    # resolve on the host — baking it in silently breaks MCP and every capture
    # hook. So for those commands we leave AI_MEMORY_SERVER_URL unset, letting
    # the CLI render its host-reachable default (http://127.0.0.1:49374).
    # (issue #107)
    if [ -z "${AI_MEMORY_SERVER_URL:-}" ] && [ "${RENDERS_HOST_CONFIG}" -eq 0 ]; then
      ENV_ARGS+=(-e "AI_MEMORY_SERVER_URL=http://host.docker.internal:49374")
    fi
    # On macOS, Docker Desktop handles file-sharing permissions via its
    # gRPC/SSH layer.  Passing -u <host-uid>:<host-gid> causes a UID
    # mismatch: the data volume is typically owned by the container's
    # internal uid 1000 (the ai-memory user), but the host UID on macOS
    # is usually 501/502.  The one-shot wrapper container then cannot
    # create log files or write to the data dir, crashing with
    # "Permission denied" in the rolling file appender.
    # Omitting -u lets the container run as its default (uid 1000)
    # which matches the volume owner. Keep the earlier rootless-Docker
    # exception for commands that touch host binds: under rootless Docker,
    # UID 0 maps back to the invoking host user and is the only mapping that
    # can access bind-mounted host files reliably.
    if ! { [ "${ROOTLESS_DOCKER}" -eq 1 ] && [ "${TOUCHES_HOST_FILES}" -eq 1 ]; }; then
      USER_ARGS=()
    fi
    ;;
esac

# Mount the data dir at /data so commands that open the store
# (status, bootstrap, search, write-page, lint, embed, …) see the
# same content the server sees. Bind-mount a host path if the user
# overrode AI_MEMORY_DATA_DIR; otherwise use the named volume.
DATA_ARGS=(-e "AI_MEMORY_DATA_DIR=/data")
if [ "${HOST_DATA_BIND}" -eq 1 ]; then
  DATA_ARGS+=(-v "${AI_MEMORY_DATA_DIR}:/data")
else
  DATA_ARGS+=(-v "${DATA_VOLUME}:/data")
fi

# `AI_MEMORY_HOST_CWD` keeps project identity stable inside the helper, while
# `AI_MEMORY_SCOPE_CWD` names the same directory inside a bounded read-only
# mount used for marker discovery. $HOME is already mounted below. For
# checkouts outside it, expose no more than the nearest git root (or the exact
# cwd outside git), matching the marker walk's trust bound.
SCOPE_ARGS=()
case "${PWD}" in
  "${HOME}"|"${HOME}"/*) ;;
  *)
    SCOPE_ROOT="${PWD}"
    if command -v git >/dev/null 2>&1; then
      DETECTED_SCOPE_ROOT=$(git -C "${PWD}" rev-parse --show-toplevel 2>/dev/null || true)
      [ -n "${DETECTED_SCOPE_ROOT}" ] && SCOPE_ROOT="${DETECTED_SCOPE_ROOT}"
    fi
    case "${PWD}" in
      "${SCOPE_ROOT}") SCOPE_REL="" ;;
      "${SCOPE_ROOT}"/*) SCOPE_REL="${PWD#"${SCOPE_ROOT}"}" ;;
      *) SCOPE_ROOT="${PWD}"; SCOPE_REL="" ;;
    esac
    SCOPE_ARGS+=(
      -v "${SCOPE_ROOT}:/scope:ro"
      -e "AI_MEMORY_SCOPE_CWD=/scope${SCOPE_REL}"
    )
    ;;
esac

# Mount $HOME at the same path inside the container so:
#   - dirs::home_dir() resolves identically (Claude's default paths and any
#     CLAUDE_CONFIG_DIR beneath $HOME work without extra path flags)
#   - install-hooks stages scripts into ~/.local/share/ai-memory/hooks/
#
# Mount $PWD at /work and set the container workdir there. This
# matters for bootstrap, which needs to find a `.git` at the CWD.
# We *don't* use `-w "$PWD"` because rootless docker / runc errors
# with "mkdir <path>: file exists" when the workdir sits beneath
# an already-bind-mounted parent (the host's $HOME mount). The
# /work path is fresh inside the container so the bind mount
# resolves cleanly. Commands that don't care about the repo CWD
# (install-mcp, install-hooks, status, search, …) just ignore it.
#
# Because the container sees `/work` (not the host's actual path),
# `basename(cwd)` would resolve to "work" for every invocation if the
# CLI used it directly. We pass `AI_MEMORY_HOST_CWD=$PWD` so the
# binary can derive the *real* host-side project name from it. The
# `commands::resolve_project_name` helper checks this env var first
# and falls back to the cwd basename only if it's unset.
HELPER_ARGS=(run --rm)
# Bash 3.2 (still shipped by macOS) rejects an empty-array expansion under
# `set -u`. The `array[@]+...` form appends only arrays with at least one item.
HELPER_ARGS+=(${TTY_ARGS[@]+"${TTY_ARGS[@]}"})
HELPER_ARGS+=(${NETWORK_ARGS[@]+"${NETWORK_ARGS[@]}"})
HELPER_ARGS+=(${SELINUX_ARGS[@]+"${SELINUX_ARGS[@]}"})
HELPER_ARGS+=(
  -v "${HOME}:${HOME}"
  -v "${PWD}:/work"
  -w /work
  -e HOME="${HOME}"
  -e AI_MEMORY_HOST_CWD="${PWD}"
)
HELPER_ARGS+=(${SCOPE_ARGS[@]+"${SCOPE_ARGS[@]}"})
HELPER_ARGS+=(${USER_ARGS[@]+"${USER_ARGS[@]}"})
HELPER_ARGS+=("${DATA_ARGS[@]}")
HELPER_ARGS+=(${ENV_ARGS[@]+"${ENV_ARGS[@]}"})
HELPER_ARGS+=("${IMAGE}" "$@")

# The native binary renders completions into a buffer so a short consumer such
# as `head` can close stdout without surfacing an error. Docker's streaming
# client does not share that behavior: it reports its own broken pipe before the
# container can return success. Buffer this bounded, read-only command outside
# Docker, then preserve the same quiet SIGPIPE behavior while streaming the
# completed script to the caller. Docker failures still return before any
# partial completion script is printed.
if [ "${WRAPPER_SUBCOMMAND}" = "completions" ]; then
  COMPLETIONS_TMP=$(mktemp "${TMPDIR:-/tmp}/ai-memory-completions.XXXXXX")
  trap 'rm -f "${COMPLETIONS_TMP}"' EXIT
  if "${DOCKER}" "${HELPER_ARGS[@]}" >"${COMPLETIONS_TMP}"; then
    DOCKER_STATUS=0
  else
    DOCKER_STATUS=$?
  fi
  if [ "${DOCKER_STATUS}" -ne 0 ]; then
    exit "${DOCKER_STATUS}"
  fi
  if cat "${COMPLETIONS_TMP}"; then
    OUTPUT_STATUS=0
  else
    OUTPUT_STATUS=$?
  fi
  rm -f "${COMPLETIONS_TMP}"
  trap - EXIT
  [ "${OUTPUT_STATUS}" -eq 141 ] && exit 0
  exit "${OUTPUT_STATUS}"
fi

exec "${DOCKER}" "${HELPER_ARGS[@]}"
