//! The request manifest `ai-memory run` hands ai-jail 3.0.1+ with
//! `--request-file` (docs/design-yolo-safety-ai-jail.md §6).
//!
//! ai-jail accepts a manifest only from an absolute, user-owned regular file
//! outside the checkout with one link and no group or world write, so a
//! repository can never pose as the wrapper. It lives in ai-memory's data
//! directory, one file per checkout, written atomically (a rename leaves one
//! link) with owner-only permissions.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use sha2::{Digest as _, Sha256};

const DIR: &str = "jail-requests";

/// Where this checkout's manifest lives.
pub(crate) fn path(data_dir: &Path, checkout: &str) -> PathBuf {
    let digest = Sha256::digest(checkout.as_bytes());
    let name: String = digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    data_dir.join(DIR).join(format!("{name}.json"))
}

/// Write `manifest` for this checkout and return its path.
pub(crate) fn write(data_dir: &Path, checkout: &str, manifest: &str) -> Result<PathBuf> {
    let file = path(data_dir, checkout);
    let dir = data_dir.join(DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {}", dir.display()))?;
    }
    // `write_atomic` stages in a tempfile, which is created owner-only.
    ai_memory_wiki::write_atomic(&file, manifest.as_bytes())
        .with_context(|| format!("writing {}", file.display()))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_checkout_gets_its_own_owner_only_single_link_file() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path();
        let one = write(data, "checkout-a", "{\"version\":1}\n").unwrap();
        let two = write(data, "checkout-b", "{\"version\":1}\n").unwrap();
        assert_ne!(one, two);
        assert!(one.is_absolute());
        assert!(one.starts_with(data.join(DIR)));
        assert_eq!(std::fs::read_to_string(&one).unwrap(), "{\"version\":1}\n");
        // Rewriting replaces the content in place of the same path.
        let again = write(data, "checkout-a", "{\"version\":1,\"x\":2}\n").unwrap();
        assert_eq!(again, one);
        assert_eq!(
            std::fs::read_to_string(&one).unwrap(),
            "{\"version\":1,\"x\":2}\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            let meta = std::fs::metadata(&one).unwrap();
            assert_eq!(meta.permissions().mode() & 0o077, 0, "owner-only file");
            assert_eq!(meta.nlink(), 1, "one link");
            let dir = std::fs::metadata(data.join(DIR)).unwrap();
            assert_eq!(dir.permissions().mode() & 0o077, 0, "owner-only directory");
        }
    }

    /// The installed ai-jail when it speaks the wrapper contract; these tests
    /// check ai-memory's real manifest and flags against the real binary, so
    /// the fake in the end-to-end suite cannot drift from it unnoticed.
    #[cfg(unix)]
    fn real_ai_jail() -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        let ai_jail = std::env::split_paths(&path)
            .map(|dir| dir.join("ai-jail"))
            .find(|candidate| candidate.is_file())?;
        let support = ai_memory_workstream::ai_jail_support(&ai_jail).ok()?;
        ai_memory_workstream::ai_jail_contract(&ai_jail, &support)?;
        Some(ai_jail)
    }

    /// `status --json` from the real ai-jail for this launch, run in a fresh
    /// project and home so no real policy is read and nothing real is written.
    #[cfg(unix)]
    fn real_status(
        ai_jail: &Path,
        root: &Path,
        request_file: &Path,
    ) -> (
        std::process::Output,
        Option<ai_memory_workstream::JailStatus>,
    ) {
        let project = root.join("project");
        let home = root.join("home");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let flags = ai_memory_workstream::contract_flags(
            "claude",
            request_file,
            ai_memory_workstream::ServerReach::Loopback { port: 49374 },
            &[],
            &[],
        );
        let args = ai_memory_workstream::contract_status_invocation(
            &flags,
            Path::new("/usr/bin/ai-memory"),
            &["run".into(), "claude".into()],
        );
        let output = std::process::Command::new(ai_jail)
            .args(&args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("AI_MEMORY_SERVER_URL", "http://127.0.0.1:49374")
            .current_dir(&project)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let status = ai_memory_workstream::parse_status(&String::from_utf8_lossy(&output.stdout));
        (output, status)
    }

    #[cfg(unix)]
    #[test]
    fn the_real_ai_jail_accepts_the_manifest_and_flags_and_writes_nothing() {
        let Some(ai_jail) = real_ai_jail() else {
            eprintln!("skipping: no ai-jail 3.0.1+ on PATH");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let manifest =
            ai_memory_workstream::request_manifest(&ai_memory_workstream::contract_requests(&[
                "AI_MEMORY_SERVER_URL",
            ]));
        let file = write(&root.join("data"), "checkout", &manifest).unwrap();
        let (output, status) = real_status(&ai_jail, &root, &file);
        let status = status.unwrap_or_else(|| {
            panic!(
                "ai-jail refused the manifest: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(
            status.pending,
            ["agent_state true", "env AI_MEMORY_SERVER_URL"],
            "every request is pending until the user approves it"
        );
        assert!(!status.onboarding_complete);
        assert!(
            status
                .blockers
                .iter()
                .any(|(code, _)| code == "required_grant_not_approved"),
            "{status:?}"
        );
        // Read-only: neither the fresh home nor the project gained a file.
        for dir in ["home", "project"] {
            assert_eq!(
                std::fs::read_dir(root.join(dir)).unwrap().count(),
                0,
                "{dir} was written"
            );
        }
    }

    /// Adversarial: a manifest the repository could plant or another user
    /// could edit is refused by ai-jail itself, which is why ai-memory keeps
    /// it in its data directory, owner-only and single-link. The control is
    /// the same manifest written by [`write`].
    #[cfg(unix)]
    #[test]
    fn the_real_ai_jail_refuses_a_manifest_in_the_checkout_or_writable_by_others() {
        use std::os::unix::fs::PermissionsExt as _;
        let Some(ai_jail) = real_ai_jail() else {
            eprintln!("skipping: no ai-jail 3.0.1+ on PATH");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let manifest =
            ai_memory_workstream::request_manifest(&ai_memory_workstream::contract_requests(&[]));
        let control = write(&root.join("data"), "checkout", &manifest).unwrap();
        assert!(
            real_status(&ai_jail, &root, &control).1.is_some(),
            "control"
        );

        let inside = root.join("project").join("requests.json");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        std::fs::write(&inside, &manifest).unwrap();
        std::fs::set_permissions(&inside, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (output, status) = real_status(&ai_jail, &root, &inside);
        assert!(
            status.is_none() && !output.status.success(),
            "a checkout manifest was accepted"
        );

        let shared = root.join("shared.json");
        std::fs::write(&shared, &manifest).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o666)).unwrap();
        let (output, status) = real_status(&ai_jail, &root, &shared);
        assert!(
            status.is_none() && !output.status.success(),
            "a world-writable manifest was accepted"
        );

        let linked = root.join("linked.json");
        std::os::unix::fs::symlink(&control, &linked).unwrap();
        let (output, status) = real_status(&ai_jail, &root, &linked);
        assert!(
            status.is_none() && !output.status.success(),
            "a symlinked manifest was accepted"
        );
    }
}
