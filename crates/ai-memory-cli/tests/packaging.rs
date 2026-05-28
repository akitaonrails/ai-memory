//! Packaging asset regression tests.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate should live under crates/ai-memory-cli")
        .to_path_buf()
}

fn read_repo(path: &str) -> String {
    let path = repo_root().join(path);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

#[test]
fn systemd_units_use_explicit_native_paths() {
    let system = read_repo("packaging/systemd/ai-memory.service");
    assert!(system.contains("--data-dir /var/lib/ai-memory"));
    assert!(system.contains("--config /etc/ai-memory/config.toml"));
    assert!(system.contains("EnvironmentFile=-/etc/ai-memory/env"));
    assert!(system.contains("StateDirectory=ai-memory"));
    assert!(system.contains("ReadWritePaths=/var/lib/ai-memory"));
    assert!(!system.contains("/var/local"));

    let user = read_repo("packaging/systemd/ai-memory-user.service");
    assert!(user.contains("--data-dir %h/.local/share/ai-memory"));
    assert!(user.contains("--config %h/.config/ai-memory/config.toml"));
    assert!(user.contains("EnvironmentFile=-%h/.config/ai-memory/env"));
    assert!(!user.contains("/var/lib/ai-memory"));
}

#[test]
fn aur_packages_install_all_native_assets() {
    for path in ["packaging/aur/PKGBUILD", "packaging/aur/PKGBUILD-bin"] {
        let pkgbuild = read_repo(path);
        assert!(pkgbuild.contains("/usr/bin/ai-memory"), "{path}");
        assert!(pkgbuild.contains("/usr/share/ai-memory"), "{path}");
        assert!(
            pkgbuild.contains("/usr/lib/systemd/system/ai-memory.service"),
            "{path}"
        );
        assert!(
            pkgbuild.contains("/usr/lib/systemd/user/ai-memory.service"),
            "{path}"
        );
        assert!(
            pkgbuild.contains("/usr/lib/sysusers.d/ai-memory.conf"),
            "{path}"
        );
        assert!(
            pkgbuild.contains("/usr/lib/tmpfiles.d/ai-memory.conf"),
            "{path}"
        );
        assert!(pkgbuild.contains("etc/ai-memory/config.toml"), "{path}");
        assert!(pkgbuild.contains("etc/ai-memory/env"), "{path}");
        assert!(
            pkgbuild.contains("install -Dm0640 packaging/env/ai-memory.env"),
            "{path}"
        );
    }

    let install = read_repo("packaging/aur/ai-memory.install");
    assert!(install.contains("sudo -u ai-memory ai-memory --data-dir /var/lib/ai-memory"));
    assert!(!install.contains("sudo ai-memory --data-dir /var/lib/ai-memory"));

    let bin_pkgbuild = read_repo("packaging/aur/PKGBUILD-bin");
    assert!(bin_pkgbuild.contains("source_x86_64"));
    assert!(bin_pkgbuild.contains("source_aarch64"));
    assert!(bin_pkgbuild.contains("linux-x86_64.tar.gz"));
    assert!(bin_pkgbuild.contains("linux-aarch64.tar.gz"));
}

#[test]
fn docker_source_build_uses_vendored_tailwind() {
    let dockerfile = read_repo("docker/Dockerfile");
    assert!(dockerfile.contains("TAILWIND_SKIP=1 cargo build --release -p ai-memory-cli"));
}

#[test]
fn docker_publish_jobs_use_prebuilt_binaries() {
    let dockerfile = read_repo("docker/Dockerfile");
    assert!(dockerfile.contains("FROM runtime-base AS runtime-prebuilt-amd64"));
    assert!(dockerfile.contains("FROM runtime-base AS runtime-prebuilt-arm64"));
    assert!(dockerfile.contains("dist/docker/ai-memory-linux-x86_64/ai-memory"));
    assert!(dockerfile.contains("dist/docker/ai-memory-linux-aarch64/ai-memory"));

    let release = read_repo(".github/workflows/release.yml");
    assert!(release.contains("artifact: ai-memory-linux-x86_64"));
    assert!(release.contains("artifact: ai-memory-linux-aarch64"));
    assert!(release.contains("target: runtime-prebuilt-amd64"));
    assert!(release.contains("target: runtime-prebuilt-arm64"));

    let ci = read_repo(".github/workflows/ci.yml");
    assert!(ci.contains("ci-ai-memory-linux-x86_64"));
    assert!(ci.contains("--target runtime-prebuilt-amd64"));
}
