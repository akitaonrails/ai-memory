//! Hermetic end-to-end coverage for native `ai-memory upgrade`.
//!
//! Unit tests in `commands/upgrade.rs` cover checksum / extract / atomic
//! replace in isolation. This drives the shipped binary through the same
//! path an operator hits: download from a fixture Releases base → verify →
//! replace the on-disk exe → refresh a sibling `hooks/` tree. No real
//! GitHub, no Docker.
//!
//! Covers Unix `.tar.gz` and Windows `.zip` release layouts.

/// Spawns a loopback release fixture and a real CLI subprocess: seconds,
/// not milliseconds — slow tier (`cargo tf` / CI), not the everyday loop.
mod slow {
    use std::fs;
    use std::io::{Cursor, Write as _};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::{Path as AxumPath, State};
    use axum::http::{StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use sha2::{Digest as _, Sha256};
    use tar::Header;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use crate::e2e_support::hermetic;

    const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
    const TAG: &str = "v9.9.9";
    /// Distinct bytes so we can prove the on-disk binary was swapped.
    const NEW_BINARY: &[u8] = b"#!/bin/sh\necho upgraded-native-e2e\n";
    const NEW_HOOK: &[u8] = b"#!/bin/sh\necho refreshed-hook\n";
    const OLD_HOOK: &[u8] = b"old-hook-body\n";

    #[derive(Clone)]
    struct ReleaseFixture {
        asset: String,
        archive: Arc<Vec<u8>>,
        checksum_body: String,
    }

    fn host_asset_name() -> &'static str {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => "ai-memory-linux-x86_64.tar.gz",
            ("linux", "aarch64") => "ai-memory-linux-aarch64.tar.gz",
            ("macos", "aarch64") => "ai-memory-macos-aarch64.tar.gz",
            ("macos", "x86_64") => "ai-memory-macos-x86_64.tar.gz",
            ("windows", "x86_64") => "ai-memory-windows-x86_64.zip",
            other => panic!("native upgrade e2e unsupported on {other:?}"),
        }
    }

    fn shipped_binary_name() -> &'static str {
        if cfg!(windows) {
            "ai-memory.exe"
        } else {
            "ai-memory"
        }
    }

    fn build_release_archive() -> Vec<u8> {
        if cfg!(windows) {
            build_release_zip()
        } else {
            build_release_tarball()
        }
    }

    fn build_release_tarball() -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            append_regular(&mut builder, "ai-memory", NEW_BINARY, 0o755);
            append_regular(&mut builder, "hooks/claude-code/new.sh", NEW_HOOK, 0o755);
            // Match release.yml Unix layout (exact support paths) so hermetic
            // upgrade still exercises the #1025 support-file allowlist.
            append_dir(&mut builder, "packaging");
            append_dir(&mut builder, "packaging/launchd");
            append_regular(
                &mut builder,
                "packaging/launchd/com.github.akitaonrails.ai-memory.plist",
                b"plist",
                0o644,
            );
            append_dir(&mut builder, "docs");
            append_regular(&mut builder, "docs/install.md", b"# install", 0o644);
            append_dir(&mut builder, "crates");
            append_regular(
                &mut builder,
                "crates/ai-memory-cli/templates/config.default.toml",
                b"# config",
                0o644,
            );
            append_regular(&mut builder, "README.md", b"# ai-memory", 0o644);
            append_regular(&mut builder, "LICENSE", b"MIT", 0o644);
            builder.finish().expect("finish tar");
        }
        let mut gz_bytes = Vec::new();
        {
            let mut enc = GzEncoder::new(&mut gz_bytes, Compression::fast());
            enc.write_all(&tar_bytes).expect("gzip write");
            enc.finish().expect("gzip finish");
        }
        gz_bytes
    }

    fn build_release_zip() -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut cursor);
            let options =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            zip.start_file("ai-memory.exe", options).expect("start exe");
            zip.write_all(NEW_BINARY).expect("write exe");
            zip.add_directory("hooks/", options).expect("hooks dir");
            zip.start_file("hooks/claude-code/new.sh", options)
                .expect("start hook");
            zip.write_all(NEW_HOOK).expect("write hook");
            // Windows release.yml ships docs/crates/README/LICENSE (no
            // packaging/); include them so zip allowlist drift is caught.
            zip.add_directory("docs/", options).expect("docs dir");
            zip.start_file("docs/install.md", options)
                .expect("start docs");
            zip.write_all(b"# install").expect("write docs");
            zip.add_directory("crates/", options).expect("crates dir");
            zip.start_file(
                "crates/ai-memory-cli/templates/config.default.toml",
                options,
            )
            .expect("start crates");
            zip.write_all(b"# config").expect("write crates");
            zip.start_file("README.md", options).expect("start readme");
            zip.write_all(b"# ai-memory").expect("write readme");
            zip.start_file("LICENSE", options).expect("start license");
            zip.write_all(b"MIT").expect("write license");
            zip.finish().expect("finish zip");
        }
        cursor.into_inner()
    }

    fn append_dir(builder: &mut tar::Builder<&mut Vec<u8>>, path: &str) {
        let mut header = Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_path(path).expect("set dir path");
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append(&header, std::io::empty())
            .expect("append tar dir");
    }

    fn append_regular(
        builder: &mut tar::Builder<&mut Vec<u8>>,
        path: &str,
        body: &[u8],
        mode: u32,
    ) {
        let mut header = Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_path(path).expect("set path");
        header.set_size(body.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        builder.append(&header, body).expect("append tar entry");
    }

    async fn latest_tag() -> &'static str {
        TAG
    }

    async fn download(
        AxumPath((tag, name)): AxumPath<(String, String)>,
        State(fx): State<ReleaseFixture>,
    ) -> Response {
        if tag != TAG {
            return (StatusCode::NOT_FOUND, "unknown tag").into_response();
        }
        let checksum_name = format!("{}.sha256", fx.asset);
        if name == checksum_name {
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/plain")],
                fx.checksum_body.clone(),
            )
                .into_response();
        }
        if name == fx.asset {
            let content_type = if fx.asset.ends_with(".zip") {
                "application/zip"
            } else {
                "application/gzip"
            };
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(fx.archive.as_ref().clone()))
                .expect("build archive response");
        }
        (StatusCode::NOT_FOUND, "unknown asset").into_response()
    }

    fn install_writable_prefix(prefix: &Path) -> PathBuf {
        let bin_dir = prefix.join("bin");
        fs::create_dir_all(&bin_dir).expect("bin dir");
        let exe = bin_dir.join(shipped_binary_name());
        fs::copy(BIN, &exe).expect("copy built binary into writable prefix");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("chmod +x");
        }

        // Sibling hooks tree: upgrade refreshes it when the archive ships hooks/.
        let hooks = bin_dir.join("hooks/claude-code");
        fs::create_dir_all(&hooks).expect("sibling hooks");
        fs::write(hooks.join("old.sh"), OLD_HOOK).expect("plant old hook");
        exe
    }

    #[tokio::test]
    async fn native_upgrade_replaces_binary_and_sibling_hooks_from_fixture() {
        let asset = host_asset_name().to_string();
        let archive = Arc::new(build_release_archive());
        let hash = format!("{:x}", Sha256::digest(archive.as_ref()));
        let checksum_body = format!("{hash}  {asset}\n");
        let fixture = ReleaseFixture {
            asset: asset.clone(),
            archive: Arc::clone(&archive),
            checksum_body,
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind release fixture");
        let addr = listener.local_addr().expect("fixture addr");
        let app = Router::new()
            .route("/releases/latest/tag", get(latest_tag))
            .route("/releases/download/{tag}/{name}", get(download))
            .with_state(fixture);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("fixture serve");
        });

        let prefix = tempfile::tempdir().expect("install prefix");
        let data_dir = tempfile::tempdir().expect("data dir");
        let home = tempfile::tempdir().expect("home");
        let exe = install_writable_prefix(prefix.path());
        let before = fs::read(&exe).expect("read pre-upgrade binary");
        assert_ne!(
            before, NEW_BINARY,
            "pre-upgrade binary must differ from fixture payload"
        );

        let base = format!("http://{addr}/releases");
        // Async child I/O — a blocking `.output()` would starve the in-process
        // fixture on the current-thread tokio test runtime.
        let mut cmd = hermetic(exe.to_str().expect("utf-8 exe path"));
        cmd.args(["upgrade", "--version", TAG, "--force"])
            .env("AI_MEMORY_RELEASE_BASE_URL", &base)
            .env("AI_MEMORY_DATA_DIR", data_dir.path())
            .env("AI_MEMORY_HOME", home.path());
        let output = tokio::process::Command::from(cmd)
            .output()
            .await
            .expect("spawn upgrade");

        assert!(
            output.status.success(),
            "upgrade failed: stdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(&format!("upgraded to {TAG}")),
            "missing success line: {stdout}"
        );

        let after = fs::read(&exe).expect("read post-upgrade binary");
        assert_eq!(after, NEW_BINARY, "on-disk binary was not replaced");

        let hooks_root = prefix.path().join("bin/hooks");
        assert_eq!(
            fs::read(hooks_root.join("claude-code/new.sh")).expect("new hook"),
            NEW_HOOK
        );
        assert!(
            !hooks_root.join("claude-code/old.sh").exists(),
            "sibling hooks tree should be fully replaced"
        );

        server.abort();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn native_upgrade_refreshes_hooks_without_a_deleted_path_or_losing_cli_data_dir() {
        let asset = host_asset_name().to_string();
        let archive = Arc::new(build_release_archive());
        let hash = format!("{:x}", Sha256::digest(archive.as_ref()));
        let fixture = ReleaseFixture {
            checksum_body: format!("{hash}  {asset}\n"),
            asset,
            archive,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind release fixture");
        let addr = listener.local_addr().expect("fixture addr");
        let app = Router::new()
            .route("/releases/latest/tag", get(latest_tag))
            .route("/releases/download/{tag}/{name}", get(download))
            .with_state(fixture);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("fixture serve");
        });

        let prefix = tempfile::tempdir().expect("install prefix");
        let data_dir = tempfile::tempdir().expect("data dir");
        let home = tempfile::tempdir().expect("home");
        let exe = install_writable_prefix(prefix.path());
        // This case needs install-hooks to resolve its source from the staged
        // data dir after replacement, not from the intentionally minimal
        // sibling hooks fixture used by the replacement test above.
        fs::remove_dir_all(prefix.path().join("bin/hooks")).expect("remove sibling hooks");
        let hooks_source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .join("hooks");
        let settings = home.path().join(".claude/settings.json");

        let mut install = hermetic(exe.to_str().expect("utf-8 exe path"));
        install
            .arg("--data-dir")
            .arg(data_dir.path())
            .args(["install-hooks", "--agent", "claude-code", "--hooks-dir"])
            .arg(&hooks_source)
            .arg("--apply")
            .env("HOME", home.path());
        let installed = tokio::process::Command::from(install)
            .output()
            .await
            .expect("install initial hooks");
        assert!(
            installed.status.success(),
            "initial hook install failed: {}",
            String::from_utf8_lossy(&installed.stderr)
        );

        // Force proof that the post-replacement refresh actually rewrites the
        // config. A child process that loses the CLI-only --data-dir, or a
        // no-op refresh, leaves this exact reported corruption behind.
        let clean = fs::read_to_string(&settings).expect("read initial settings");
        let exe_text = exe.to_string_lossy();
        assert!(clean.contains(exe_text.as_ref()), "missing native exe path");
        let corrupted = clean.replace(exe_text.as_ref(), &format!("{} (deleted)", exe.display()));
        assert_ne!(clean, corrupted, "fixture did not corrupt any hook path");
        fs::write(&settings, corrupted).expect("plant deleted-path suffix");

        let base = format!("http://{addr}/releases");
        let mut upgrade = hermetic(exe.to_str().expect("utf-8 exe path"));
        upgrade
            .arg("--data-dir")
            .arg(data_dir.path())
            .args(["upgrade", "--version", TAG, "--force"])
            .env("AI_MEMORY_RELEASE_BASE_URL", &base)
            .env("HOME", home.path());
        let output = tokio::process::Command::from(upgrade)
            .output()
            .await
            .expect("spawn upgrade");
        assert!(
            output.status.success(),
            "upgrade failed: stdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let refreshed = fs::read_to_string(&settings).expect("read refreshed settings");
        assert!(!refreshed.contains(" (deleted)"), "{refreshed}");
        assert!(refreshed.contains(exe_text.as_ref()), "{refreshed}");
        assert!(
            refreshed.contains(&data_dir.path().to_string_lossy().into_owned()),
            "CLI-only data dir was lost: {refreshed}"
        );
        server.abort();
    }
}
