# AUR Packaging

This directory contains the package definitions for two Arch User Repository
packages:

- `ai-memory-bin`: installs the prebuilt Linux x86_64/aarch64 binary and bundled
  files from GitHub Releases.
- `ai-memory`: builds from the GitHub source tag with the local Rust toolchain.

The files here are the source of truth for the AUR repos, but AUR still requires
each package to be pushed to its own Git repository with a generated `.SRCINFO`.

## User Install

Use an AUR helper:

```bash
yay -S ai-memory-bin    # prebuilt binary, fastest on x86_64/aarch64
yay -S ai-memory        # builds from source, supports x86_64/aarch64
```

Then follow the native Linux service instructions in `docs/install.md`.

## Local Integration Test

Before publishing package updates, run the manual distrobox harness from the
repository root:

```bash
scripts/test-native-arch-systemd-distrobox.sh
```

It creates a disposable Arch distrobox, builds the current working tree,
installs the native filesystem layout, verifies the AUR metadata shape, starts
the packaged system service with real `systemctl`, smoke-tests the user profile
runner under transient systemd supervision, and confirms packaged hook sources
stage correctly.

Routine CI runs `scripts/check-native-packaging.sh` for non-destructive coverage
against a temporary alternate root. The distrobox script is the final real
service-start smoke before publishing.

## Maintainer Release Checklist

The GitHub release workflow publishes these AUR repos automatically on `v*.*.*`
tags when `AUR_SSH_PRIVATE_KEY` is configured. It validates that the tag version
matches `Cargo.toml`, computes fresh checksums, generates `.SRCINFO`, and pushes
both `ai-memory` and `ai-memory-bin`.

For every new upstream release:

1. Bump `[workspace.package].version` in `Cargo.toml` and all workspace crate
   dependency versions as usual.
2. Run the local gate, including `scripts/check-native-packaging.sh` and the
   disposable distrobox integration test.
3. Push a matching version tag, for example `v0.3.0`.
4. Confirm the `release` workflow created the GitHub release, pushed Docker if
   Docker Hub secrets are configured, and pushed AUR if `AUR_SSH_PRIVATE_KEY` is
   configured.

Manual fallback if the AUR job is disabled or needs repair:

1. Update `pkgver` in both `PKGBUILD` files and reset `pkgrel=1`.
2. Update source checksums. Replace any temporary `SKIP` values before pushing
   to AUR.
3. Validate both package variants with `makepkg --verifysource` and `makepkg -Ccf`.
4. Generate `.SRCINFO` in each AUR checkout with `makepkg --printsrcinfo > .SRCINFO`.
5. Commit and push to the separate AUR repos: `ai-memory` and `ai-memory-bin`.

Checksum helpers for version `X.Y.Z`:

```bash
version=X.Y.Z

# Source package tarball
curl -fsSL "https://github.com/akitaonrails/ai-memory/archive/refs/tags/v${version}.tar.gz" | sha256sum

# Binary package release artifacts
curl -fsSL "https://github.com/akitaonrails/ai-memory/releases/download/v${version}/ai-memory-linux-x86_64.tar.gz.sha256"
curl -fsSL "https://github.com/akitaonrails/ai-memory/releases/download/v${version}/ai-memory-linux-aarch64.tar.gz.sha256"
```
