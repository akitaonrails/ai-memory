//! `install.sh` against a fake release served over local HTTP.
//!
//! The script must install a verified release under a temp home and must
//! refuse, before unpacking anything, a checksum mismatch, a missing or
//! foreign checksum file, and an archive with an escaping path. A copy cut
//! short in transit must run nothing.

#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use sha2::{Digest as _, Sha256};

const TARGET: &str = "linux-x86_64";
const ASSET: &str = "ai-memory-linux-x86_64.tar.gz";

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install.sh")
}

/// Serves files from `root` over HTTP/1.0, 404 for anything missing, and
/// records each request path.
struct Release {
    root: PathBuf,
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Release {
    fn serve(root: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/releases", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let files = root.to_path_buf();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut line = String::new();
                if BufReader::new(&mut stream).read_line(&mut line).is_err() {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                seen.lock().unwrap().push(path.clone());
                let local = files.join(path.trim_start_matches('/'));
                let reply = match std::fs::read(&local) {
                    Ok(body) if !path.contains("..") => {
                        let mut reply = format!(
                            "HTTP/1.0 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        reply.extend(body);
                        reply
                    }
                    _ => {
                        b"HTTP/1.0 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = stream.write_all(&reply);
            }
        });
        Self {
            root: root.to_path_buf(),
            base,
            requests,
        }
    }

    fn latest(&self) -> PathBuf {
        let dir = self.root.join("releases/latest/download");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Publish `archive` with a correct `.sha256` sidecar.
    fn publish(&self, archive: &[u8]) {
        std::fs::write(self.latest().join(ASSET), archive).unwrap();
        let sum = format!("{:x}", Sha256::digest(archive));
        self.publish_sidecar(&format!("{sum}  {ASSET}\n"));
    }

    fn publish_sidecar(&self, text: &str) {
        std::fs::write(self.latest().join(format!("{ASSET}.sha256")), text).unwrap();
    }
}

/// A release archive laid out like `release.yml` builds it, with a stub
/// binary that answers `--version`.
fn release_archive(extra: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let mut add = |name: &str, body: &[u8], mode: u32| {
        let mut header = tar::Header::new_gnu();
        let bytes = name.as_bytes();
        // Written raw so a test can plant a name `set_path` would refuse.
        header.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
        header.set_size(body.len() as u64);
        header.set_mode(mode);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append(&header, body).unwrap();
    };
    add(
        "ai-memory",
        b"#!/bin/sh\necho 'ai-memory 0.0.0-install-test'\n",
        0o755,
    );
    add("hooks/claude-code/stop.sh", b"#!/bin/sh\n", 0o755);
    for (name, body) in extra {
        add(name, body, 0o644);
    }
    builder.into_inner().unwrap().finish().unwrap()
}

struct Home {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Home {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        Self {
            _tmp: tmp,
            root,
            home,
        }
    }

    fn link(&self) -> PathBuf {
        self.home.join(".local/bin/ai-memory")
    }

    fn lib(&self) -> PathBuf {
        self.home.join(".local/lib/ai-memory")
    }

    fn command(&self, release: &Release) -> Command {
        let mut cmd = Command::new("sh");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("AI_MEMORY_") {
                cmd.env_remove(key);
            }
        }
        cmd.env("HOME", &self.home)
            .env("TMPDIR", &self.root)
            .env("AI_MEMORY_RELEASE_BASE_URL", &release.base)
            .env("AI_MEMORY_INSTALL_TARGET", TARGET)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    fn install(&self, release: &Release) -> Output {
        self.command(release).arg(script()).output().unwrap()
    }

    /// `curl … | sh`: the script arrives on stdin.
    fn pipe(&self, release: &Release, body: &[u8]) -> Output {
        let mut child = self.command(release).stdin(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(body).unwrap();
        child.wait_with_output().unwrap()
    }

    /// Nothing was installed and the work dir was cleaned up.
    fn assert_untouched(&self) {
        assert!(!self.link().exists() && self.link().symlink_metadata().is_err());
        assert!(!self.lib().exists());
        let leftovers: Vec<_> = std::fs::read_dir(&self.root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .filter(|name| name != "home")
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }
}

fn report(out: &Output) -> String {
    format!(
        "status: {}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn installs_a_verified_release_under_the_home() {
    let files = tempfile::tempdir().unwrap();
    let release = Release::serve(files.path());
    release.publish(&release_archive(&[]));
    let home = Home::new();

    let out = home.install(&release);
    assert!(out.status.success(), "{}", report(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("checksum verified"));
    assert_eq!(
        std::fs::read_link(home.link()).unwrap(),
        home.lib().join("ai-memory")
    );
    assert!(home.lib().join("hooks/claude-code/stop.sh").is_file());
    let version = Command::new(home.link()).arg("--version").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "ai-memory 0.0.0-install-test"
    );
    assert!(
        release
            .requests
            .lock()
            .unwrap()
            .contains(&format!("/releases/latest/download/{ASSET}.sha256"))
    );

    // Re-running replaces the release in place and keeps the link.
    let again = home.install(&release);
    assert!(again.status.success(), "{}", report(&again));
    assert!(home.lib().join("ai-memory").is_file());
}

#[test]
fn a_pinned_version_downloads_that_tag() {
    let files = tempfile::tempdir().unwrap();
    let release = Release::serve(files.path());
    let archive = release_archive(&[]);
    let dir = files.path().join("releases/download/v9.9.9");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(ASSET), &archive).unwrap();
    std::fs::write(
        dir.join(format!("{ASSET}.sha256")),
        format!("{:x}  {ASSET}\n", Sha256::digest(&archive)),
    )
    .unwrap();
    let home = Home::new();
    let out = home
        .command(&release)
        .env("AI_MEMORY_VERSION", "9.9.9")
        .arg(script())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", report(&out));
}

#[test]
fn a_checksum_mismatch_installs_nothing() {
    let files = tempfile::tempdir().unwrap();
    let release = Release::serve(files.path());
    release.publish(&release_archive(&[]));
    release.publish_sidecar(&format!("{}  {ASSET}\n", "0".repeat(64)));
    let home = Home::new();

    let out = home.install(&release);
    assert!(!out.status.success(), "{}", report(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("checksum mismatch"),
        "{}",
        report(&out)
    );
    home.assert_untouched();
}

#[test]
fn a_missing_checksum_file_installs_nothing() {
    let files = tempfile::tempdir().unwrap();
    let release = Release::serve(files.path());
    std::fs::write(release.latest().join(ASSET), release_archive(&[])).unwrap();
    let home = Home::new();

    let out = home.install(&release);
    assert!(!out.status.success(), "{}", report(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("without a checksum"),
        "{}",
        report(&out)
    );
    home.assert_untouched();
}

/// A sidecar copied from another asset (right hash format, wrong name) or
/// one that is not a hash at all is refused too.
#[test]
fn a_foreign_or_malformed_checksum_file_installs_nothing() {
    let files = tempfile::tempdir().unwrap();
    let release = Release::serve(files.path());
    let archive = release_archive(&[]);
    release.publish(&archive);
    for sidecar in [
        format!(
            "{:x}  ai-memory-macos-aarch64.tar.gz\n",
            Sha256::digest(&archive)
        ),
        "<html>not found</html>\n".to_owned(),
        String::new(),
    ] {
        release.publish_sidecar(&sidecar);
        let home = Home::new();
        let out = home.install(&release);
        assert!(!out.status.success(), "{sidecar:?}: {}", report(&out));
        home.assert_untouched();
    }
}

#[test]
fn an_archive_with_an_escaping_path_installs_nothing() {
    let files = tempfile::tempdir().unwrap();
    let release = Release::serve(files.path());
    release.publish(&release_archive(&[("../escaped", b"owned")]));
    let home = Home::new();

    let out = home.install(&release);
    assert!(!out.status.success(), "{}", report(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unsafe path"),
        "{}",
        report(&out)
    );
    home.assert_untouched();
    assert!(!home.root.join("escaped").exists());
}

/// Whatever sits at the link path and is not a link is the user's: a Docker
/// wrapper or a hand-built binary is never replaced.
#[test]
fn an_existing_binary_at_the_link_path_is_left_alone() {
    let files = tempfile::tempdir().unwrap();
    let release = Release::serve(files.path());
    release.publish(&release_archive(&[]));
    let home = Home::new();
    std::fs::create_dir_all(home.link().parent().unwrap()).unwrap();
    std::fs::write(home.link(), "#!/bin/sh\necho wrapper\n").unwrap();

    let out = home.install(&release);
    assert!(!out.status.success(), "{}", report(&out));
    assert_eq!(
        std::fs::read_to_string(home.link()).unwrap(),
        "#!/bin/sh\necho wrapper\n"
    );
    assert!(!home.lib().exists());
}

/// A link at the link path is replaced only when it points at this install's
/// binary. One to a hand-built binary, as docs/macos.md suggests for a source
/// build, is left alone and nothing is downloaded.
#[test]
fn a_link_to_a_hand_built_binary_is_left_alone() {
    let files = tempfile::tempdir().unwrap();
    let release = Release::serve(files.path());
    release.publish(&release_archive(&[]));
    let home = Home::new();
    let built = home.root.join("src/target/release/ai-memory");
    std::fs::create_dir_all(built.parent().unwrap()).unwrap();
    std::fs::write(&built, "#!/bin/sh\necho hand-built\n").unwrap();
    std::fs::create_dir_all(home.link().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&built, home.link()).unwrap();

    let out = home.install(&release);
    assert!(!out.status.success(), "{}", report(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&format!("links to {}", built.display())),
        "{}",
        report(&out)
    );
    assert_eq!(std::fs::read_link(home.link()).unwrap(), built);
    assert!(!home.lib().exists());
    assert!(
        release.requests.lock().unwrap().is_empty(),
        "a refused run reached the network"
    );
}

/// A reinstall moves the lib dir aside and deletes it. Pointed at the
/// directory holding the link, that deleted every other program there and
/// left `ai-memory` a link to itself; it is refused before anything is
/// downloaded.
#[test]
fn a_lib_dir_that_is_the_bin_dir_is_refused_and_left_intact() {
    let files = tempfile::tempdir().unwrap();
    let release = Release::serve(files.path());
    release.publish(&release_archive(&[]));
    let home = Home::new();
    let first = home.install(&release);
    assert!(first.status.success(), "{}", report(&first));
    let bin = home.link().parent().unwrap().to_path_buf();
    std::fs::write(bin.join("other-tool"), "#!/bin/sh\necho mine\n").unwrap();
    let requests = release.requests.lock().unwrap().len();

    let out = home
        .command(&release)
        .env("AI_MEMORY_LIB_DIR", &bin)
        .arg(script())
        .output()
        .unwrap();
    assert!(!out.status.success(), "{}", report(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is or contains the link's directory"),
        "{}",
        report(&out)
    );
    assert_eq!(
        std::fs::read_to_string(bin.join("other-tool")).unwrap(),
        "#!/bin/sh\necho mine\n"
    );
    assert_eq!(
        std::fs::read_link(home.link()).unwrap(),
        home.lib().join("ai-memory")
    );
    assert_eq!(
        release.requests.lock().unwrap().len(),
        requests,
        "a refused run reached the network"
    );
}

/// The reinstall deletes the whole lib dir, so one holding a file the release
/// does not ship is refused once the release is unpacked, and keeps the file.
#[test]
fn a_lib_dir_holding_a_file_the_release_lacks_is_left_alone() {
    let files = tempfile::tempdir().unwrap();
    let release = Release::serve(files.path());
    release.publish(&release_archive(&[]));
    let home = Home::new();
    let first = home.install(&release);
    assert!(first.status.success(), "{}", report(&first));
    std::fs::write(home.lib().join("NOTES.txt"), "mine\n").unwrap();

    let out = home.install(&release);
    assert!(!out.status.success(), "{}", report(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("holds NOTES.txt"),
        "{}",
        report(&out)
    );
    assert_eq!(
        std::fs::read_to_string(home.lib().join("NOTES.txt")).unwrap(),
        "mine\n"
    );
    assert!(home.lib().join("ai-memory").is_file());
    let siblings: Vec<String> = std::fs::read_dir(home.lib().parent().unwrap())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        siblings,
        ["ai-memory"],
        "a refused rerun left a staged copy"
    );
}

/// `curl … | sh` with the download cut short: every cut point leaves a
/// script that defines functions at most, so nothing is fetched or written.
#[test]
fn a_truncated_script_runs_nothing() {
    let files = tempfile::tempdir().unwrap();
    let release = Release::serve(files.path());
    release.publish(&release_archive(&[]));
    let mut full = Vec::new();
    std::fs::File::open(script())
        .unwrap()
        .read_to_end(&mut full)
        .unwrap();
    let main_call = full.len() - "main \"$@\"\n".len();
    assert!(
        full.ends_with(b"main \"$@\"\n"),
        "main must be the last line"
    );

    for cut in (0..main_call).step_by(97).chain([main_call]) {
        let home = Home::new();
        let _ = home.pipe(&release, &full[..cut]);
        home.assert_untouched();
    }
    assert!(
        release.requests.lock().unwrap().is_empty(),
        "a truncated script reached the network"
    );

    // Control: the whole script, piped the same way, installs.
    let home = Home::new();
    let out = home.pipe(&release, &full);
    assert!(out.status.success(), "{}", report(&out));
    assert!(home.link().exists());
}
