//! Build script for ai-memory-web.
//!
//! The shipped stylesheet is the vendored `static/tailwind.css`, checked in
//! and copied to `OUT_DIR` on every build, so a plain `cargo build` needs no
//! network and no environment variable on any platform.
//!
//! # Regenerating the stylesheet
//! `TAILWIND_BUILD=1 cargo build -p ai-memory-web` downloads the pinned
//! standalone Tailwind CLI (checksum-verified), compiles `static/input.css`
//! against `templates/`, and rewrites the vendored file for you to commit.
//! CI runs that mode on Linux and fails if the committed file is stale, which
//! is the only freshness check: mtimes on a fresh clone mean nothing.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const TAILWIND_VERSION: &str = "3.4.17";
const TAILWIND_LINUX_X64_SHA256: &str =
    "7d24f7fa191d2193b78cd5f5a42a6093e14409521908529f42d80b11fde1f1d4";
const TAILWIND_LINUX_ARM64_SHA256: &str =
    "69b1378b8133192d7d2feb12a116fa12d035594f58db3eff215879e4ad8cf39b";
const TAILWIND_MACOS_X64_SHA256: &str =
    "6cbdad74be776c087ffa5e9a057512c54898f9fe8828d3362212dfe32fc933a3";
const TAILWIND_MACOS_ARM64_SHA256: &str =
    "a1d0c7985759accca0bf12e51ac1dcbf0f6cf2fffb62e6e0f62d091c477a10a3";
const TAILWIND_WINDOWS_X64_SHA256: &str =
    "67f1c5e3f5a03406a7bf5badf5ada09b79f3ae78ec43450c15f7e983068da346";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=static/tailwind.css");
    println!("cargo:rerun-if-env-changed=TAILWIND_BUILD");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let out_css = out_dir.join("tailwind.css");
    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendored = crate_dir.join("static/tailwind.css");

    if std::env::var("TAILWIND_BUILD").as_deref() == Ok("1") {
        println!("cargo:rerun-if-changed=static/input.css");
        println!("cargo:rerun-if-changed=templates/");
        let binary = download_tailwind(&out_dir);
        compile_tailwind(&binary, &crate_dir, &out_css);
        // This is the explicit "regenerate" mode, so refreshing the vendored
        // copy in the source tree is the point, not a side effect.
        std::fs::copy(&out_css, &vendored)
            .expect("failed to refresh the vendored static/tailwind.css");
    } else {
        std::fs::copy(&vendored, &out_css).unwrap_or_else(|e| {
            panic!(
                "static/tailwind.css missing ({e}); regenerate it with \
                 `TAILWIND_BUILD=1 cargo build -p ai-memory-web`"
            )
        });
    }

    emit_env(&out_css);
}

fn emit_env(out_css: &Path) {
    println!(
        "cargo:rustc-env=AI_MEMORY_WEB_TAILWIND_CSS={}",
        out_css.display()
    );
}

/// Platform-specific download URL for the Tailwind CLI binary.
fn tailwind_url() -> String {
    let slug = tailwind_slug();
    format!(
        "https://github.com/tailwindlabs/tailwindcss/releases/download/v{TAILWIND_VERSION}/{slug}"
    )
}

fn tailwind_slug() -> &'static str {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match (os, arch) {
        ("linux", "x86_64") => "tailwindcss-linux-x64",
        ("linux", "aarch64") => "tailwindcss-linux-arm64",
        ("macos", "x86_64") => "tailwindcss-macos-x64",
        ("macos", "aarch64") => "tailwindcss-macos-arm64",
        ("windows", "x86_64") => "tailwindcss-windows-x64.exe",
        _ => panic!(
            "Unsupported platform {os}/{arch} for TAILWIND_BUILD=1; regenerate static/tailwind.css on a supported one"
        ),
    }
}

fn expected_tailwind_sha256() -> &'static str {
    match tailwind_slug() {
        "tailwindcss-linux-x64" => TAILWIND_LINUX_X64_SHA256,
        "tailwindcss-linux-arm64" => TAILWIND_LINUX_ARM64_SHA256,
        "tailwindcss-macos-x64" => TAILWIND_MACOS_X64_SHA256,
        "tailwindcss-macos-arm64" => TAILWIND_MACOS_ARM64_SHA256,
        "tailwindcss-windows-x64.exe" => TAILWIND_WINDOWS_X64_SHA256,
        other => panic!("No pinned Tailwind SHA-256 for {other}"),
    }
}

fn verify_tailwind_checksum(path: &Path) {
    let expected = expected_tailwind_sha256();
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        panic!(
            "failed reading downloaded Tailwind binary {}: {e}",
            path.display()
        )
    });
    let got = format!("{:x}", Sha256::digest(&bytes));
    if got != expected {
        let _ = std::fs::remove_file(path);
        panic!(
            "Tailwind CSS CLI checksum mismatch for {}: expected {expected}, got {got}",
            path.display()
        );
    }
}

/// Download the Tailwind CLI and return the path to the executable.
/// The file is cached in `OUT_DIR/tailwindcss-{version}` across rebuilds.
fn download_tailwind(out_dir: &Path) -> PathBuf {
    let bin_name = if cfg!(windows) {
        format!("tailwindcss-{TAILWIND_VERSION}.exe")
    } else {
        format!("tailwindcss-{TAILWIND_VERSION}")
    };
    // Cache in the grandparent of OUT_DIR so it survives incremental rebuilds
    // across profile changes. Fall back to OUT_DIR itself if that fails.
    let cache_dir = out_dir
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .unwrap_or(out_dir);
    let dest = cache_dir.join(&bin_name);

    if dest.exists() {
        verify_tailwind_checksum(&dest);
        return dest;
    }

    let url = tailwind_url();
    eprintln!("cargo:warning=Downloading Tailwind CSS CLI v{TAILWIND_VERSION} from {url}");

    // Try ubiquitous Unix tools first, then PowerShell for stock Windows.
    let success = try_download_curl(&url, &dest)
        || try_download_wget(&url, &dest)
        || try_download_powershell(&url, &dest);

    if !success {
        panic!(
            "Could not download the Tailwind CSS CLI: curl, wget, and PowerShell all failed. \
             Install one of them, or regenerate static/tailwind.css on another machine."
        );
    }

    verify_tailwind_checksum(&dest);

    // Make executable on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms).unwrap();
    }

    dest
}

fn try_download_curl(url: &str, dest: &Path) -> bool {
    Command::new("curl")
        .args(["--fail", "--silent", "--location", "--output"])
        .arg(dest)
        .arg(url)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn try_download_wget(url: &str, dest: &Path) -> bool {
    Command::new("wget")
        .args(["--quiet", "--output-document"])
        .arg(dest)
        .arg(url)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn try_download_powershell(url: &str, dest: &Path) -> bool {
    try_download_with_powershell("pwsh", url, dest)
        || try_download_with_powershell("powershell", url, dest)
}

fn try_download_with_powershell(binary: &str, url: &str, dest: &Path) -> bool {
    Command::new(binary)
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"])
        .arg(
            "$ProgressPreference='SilentlyContinue'; \
             Invoke-WebRequest -Uri $args[0] -OutFile $args[1]",
        )
        .arg(url)
        .arg(dest)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Invoke the Tailwind CLI to produce `out_css`.
fn compile_tailwind(binary: &Path, crate_dir: &Path, out_css: &Path) {
    let input_css = crate_dir.join("static/input.css");
    let config_js = crate_dir.join("tailwind.config.js");

    let status = Command::new(binary)
        .current_dir(crate_dir)
        .args([
            "--input",
            input_css.to_str().unwrap(),
            "--output",
            out_css.to_str().unwrap(),
            "--config",
            config_js.to_str().unwrap(),
            "--minify",
        ])
        .status()
        .unwrap_or_else(|e| panic!("Failed to run tailwind CLI: {e}"));

    if !status.success() {
        panic!("tailwind CSS compilation failed (exit status {status:?})");
    }
}
