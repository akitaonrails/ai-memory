# syntax=docker/dockerfile:1.7
#
# Multi-stage build for the ai-memory MCP server.
# Builder: rust:1.95-slim with the release binary cached against
#          BuildKit's cargo registry + target caches.
# Runtime: debian:bookworm-slim with a non-root `ai-memory` user that
#          owns /data. CA certificates included for outgoing HTTPS (LLM
#          providers in M6+).
#
# The default final target builds the binary from source. Release automation and
# CI can target the prebuilt runtime stages after downloading native release
# artifacts, avoiding duplicate Rust compiles in Docker jobs.

FROM rust:1.95-slim-bookworm AS builder
WORKDIR /work
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config build-essential curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock rust-toolchain.toml deny.toml ./
COPY crates ./crates
# `evals/` is a workspace member (off-tree A/B harness, not shipped
# in the image — but Cargo refuses to parse the workspace manifest
# if a declared member doesn't exist on disk). Copy the dir so the
# manifest resolves; `-p ai-memory-cli` keeps the binary build narrow.
COPY evals ./evals
# `docs/logo*.png` are include_bytes!'d by ai-memory-web's static
# routes at compile time, so they must be in the build context.
COPY docs ./docs
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/work/target \
    TAILWIND_SKIP=1 cargo build --release -p ai-memory-cli \
 && cp target/release/ai-memory /usr/local/bin/ai-memory

FROM debian:bookworm-slim AS runtime-base
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 1000 --user-group --home-dir /data \
         --shell /usr/sbin/nologin ai-memory \
 && mkdir -p /data \
 && chown -R ai-memory:ai-memory /data
# Lifecycle-hook scripts bundled into the image so docker-only users
# can `ai-memory setup-agent` without needing a repo checkout. The
# scripts live on a path scanned by `install_hooks::resolve_hooks_dir`
# (and by `setup_agent::resolve_source`).
COPY hooks /usr/local/share/ai-memory/hooks
ENV AI_MEMORY_DATA_DIR=/data
WORKDIR /data
EXPOSE 49374
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/ai-memory", "status"]
ENTRYPOINT ["/usr/local/bin/ai-memory"]
CMD ["serve", "--transport", "http", "--bind", "0.0.0.0:49374", "--enable-web"]

FROM runtime-base AS runtime-prebuilt-amd64
COPY --chmod=0755 dist/docker/ai-memory-linux-x86_64/ai-memory /usr/local/bin/ai-memory
USER ai-memory

FROM runtime-base AS runtime-prebuilt-arm64
COPY --chmod=0755 dist/docker/ai-memory-linux-aarch64/ai-memory /usr/local/bin/ai-memory
USER ai-memory

FROM runtime-base AS runtime-source
COPY --from=builder /usr/local/bin/ai-memory /usr/local/bin/ai-memory
USER ai-memory
