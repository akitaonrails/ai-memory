//! `ai-memory restore --from <tarball>` — restore a backup tarball.
//!
//! Refuses to overwrite a non-empty data dir unless `--force` is given.
//! Refuses while another `ai-memory` process is alive.
//!
//! The live data is never touched before the archive has proven usable:
//! the tarball is extracted and validated into a staging directory beside
//! the live `wiki/` and `db/`, the restored store is opened there so any
//! pending migrations run (and a corrupt snapshot fails loudly), and only
//! then are the live directories swapped out by rename. A truncated
//! archive, an entry outside the allowed layout, or a snapshot the current
//! binary cannot open therefore leaves the existing data exactly as it was
//! — the moment a restore fails is the moment the operator has no other
//! copy, so the previous state must survive it.
//!
//! A backup's `config.toml` is code-bearing: it can name commands the server
//! runs (`[auto_improve.eval]`), where consolidated memory is sent (provider
//! URLs and headers, admission webhooks) and how much is redacted
//! (`[sanitize]`). A restore therefore keeps an existing live config and saves
//! the archived one beside it as `config.toml.from-backup` for review;
//! `--with-config` restores it in place, and a data dir with no config gets
//! the archived one as before.
//!
//! # Exception to invariant §16
//!
//! `restore` is one of the documented exceptions to the rule that the CLI
//! is always a thin HTTP client. Restoration is a lifecycle operation that
//! fundamentally requires the server to be stopped: extracting a tarball
//! over a live SQLite WAL writer would corrupt the database. The sysinfo
//! guard at the top of `run` enforces this precondition by refusing to
//! proceed when any sibling `ai-memory` process is detected.

use ai_memory_store::Store;
use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use std::path::{Component, Path, PathBuf};
use tracing::{info, warn};

use crate::cli::RestoreArgs;
use crate::config::Config;
use crate::process_guard::{busy_message, sibling_processes};

/// Data-dir directories a restore replaces wholesale: whatever the archive
/// holds for each takes the live one's place, and a directory the archive
/// lacks is retired rather than merged with the archive's state. `logs/`,
/// `models/`, `raw/` and anything else beside them are never touched.
const REPLACED_DIRS: &[&str] = &["wiki", "db"];

/// The one file a restore can replace: only when the archive carries it, and
/// only with `--with-config` or when the data dir has none.
const CONFIG_FILE: &str = "config.toml";

/// Where an archived config goes when the live one is kept.
const CONFIG_FROM_BACKUP_FILE: &str = "config.toml.from-backup";

/// What a restore did with the archive's `config.toml`.
#[derive(Debug, PartialEq, Eq)]
enum ConfigOutcome {
    /// The archive carried no config; the live one, if any, is untouched.
    NotInArchive,
    /// The archived config is now the live one.
    Restored,
    /// The live config was kept and the archived one saved at this path.
    SavedBeside(PathBuf),
}

/// The archived config's place: live when asked for or when there is none,
/// otherwise beside the live one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigPlacement {
    Absent,
    Install,
    Divert,
}

/// Run the `restore` subcommand.
///
/// # Errors
/// Returns an error if another `ai-memory` process is running, the
/// data dir is non-empty without `--force`, the tarball cannot be
/// extracted, or the restored store fails to open. In every one of those
/// cases the live data dir is left as it was.
pub async fn run(config: &Config, args: RestoreArgs) -> Result<()> {
    let i18n = &config.i18n;
    let siblings = sibling_processes();
    if !siblings.is_empty() {
        bail!(busy_message("restore", &siblings));
    }

    if !args.from.is_file() {
        bail!(
            "{}",
            i18n.format(
                "cli-restore-source-tarball-not-found",
                &[("value0", format!("{}", args.from.display()))]
            )
        );
    }

    let outcome = restore_data_dir_with_legacy_pepper(
        &config.i18n,
        &config.data_dir,
        &args.from,
        args.force,
        args.with_config,
        config.auth.token_pepper.as_deref(),
        args.accept_legacy_api_key_pepper,
    )
    .await?;
    info!("restore complete");
    println!(
        "{}",
        i18n.format(
            "cli-restore-restored",
            &[
                ("value0", format!("{}", args.from.display())),
                ("value1", format!("{}", config.data_dir.display()))
            ]
        )
    );
    match outcome {
        ConfigOutcome::NotInArchive => {}
        ConfigOutcome::Restored => println!("{}", i18n.text("cli-restore-restored-the-backup-s-config-toml")),
        ConfigOutcome::SavedBeside(path) => println!(
            "{}", i18n.format("cli-restore-kept-the-live-config-toml-the-backup-s-copy-is-at-a-config-can-run-commands", &[("value0", format!("{}", path.display()))])),
    }
    Ok(())
}

/// Stage, validate, then swap. Nothing under `data_dir` changes until the
/// archive has been fully extracted into a staging directory and the
/// staged store has opened; the live `wiki/`, `db/` and (when it is being
/// restored) `config.toml` are then exchanged for the staged copies by
/// rename and rolled back if any step of the exchange fails.
#[cfg(test)]
async fn restore_data_dir(
    i18n: &ai_memory_core::i18n::Localizer,
    data_dir: &Path,
    from: &Path,
    force: bool,
    with_config: bool,
) -> Result<ConfigOutcome> {
    restore_data_dir_with_legacy_pepper(i18n, data_dir, from, force, with_config, None, false).await
}

async fn restore_data_dir_with_legacy_pepper(
    i18n: &ai_memory_core::i18n::Localizer,
    data_dir: &Path,
    from: &Path,
    force: bool,
    with_config: bool,
    token_pepper: Option<&str>,
    accept_legacy_api_key_pepper: bool,
) -> Result<ConfigOutcome> {
    let wiki = data_dir.join("wiki");
    let db = data_dir.join("db").join("memory.sqlite");
    let occupied = (wiki.is_dir() && std::fs::read_dir(&wiki)?.next().is_some()) || db.is_file();
    if occupied && !force {
        bail!(
            "refusing to restore: data dir at {} is non-empty (pass --force to overwrite)",
            data_dir.display(),
        );
    }
    std::fs::create_dir_all(data_dir)?;

    // Both scratch directories live inside the data dir so every move below
    // is a rename on one filesystem, never a copy that could half-complete.
    let stamp = format!(
        "{}-{}",
        jiff::Timestamp::now().strftime("%Y%m%d-%H%M%S"),
        std::process::id()
    );
    let staging = data_dir.join(format!(".restore-staging-{stamp}"));
    let previous = data_dir.join(format!(".restore-previous-{stamp}"));

    let outcome = stage_then_swap(
        i18n,
        data_dir,
        from,
        &staging,
        &previous,
        with_config,
        token_pepper,
        accept_legacy_api_key_pepper,
    )
    .await;
    // The staging dir is scratch in every outcome: on success its contents
    // were moved into place, on failure the live data was never touched.
    if staging.exists()
        && let Err(e) = std::fs::remove_dir_all(&staging)
    {
        warn!(path = %staging.display(), error = %e, "could not remove restore staging dir");
        eprintln!(
            "{}",
            i18n.diagnostic(&format!(
                "warning: could not remove staging dir {} ({e}); delete it by hand",
                staging.display()
            ))
        );
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn stage_then_swap(
    i18n: &ai_memory_core::i18n::Localizer,
    data_dir: &Path,
    from: &Path,
    staging: &Path,
    previous: &Path,
    with_config: bool,
    token_pepper: Option<&str>,
    accept_legacy_api_key_pepper: bool,
) -> Result<ConfigOutcome> {
    std::fs::create_dir(staging)
        .with_context(|| format!("creating staging dir {}", staging.display()))?;

    // 1. Extract into staging, validating every entry on the way. A
    //    truncated gzip stream, an unreadable member or a path outside the
    //    allowed layout fails here, with the live data still untouched.
    let file = std::fs::File::open(from).with_context(|| format!("opening {}", from.display()))?;
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    unpack_checked_archive(&mut archive, staging)
        .with_context(|| format!("extracting {} into {}", from.display(), staging.display()))?;
    // tar stops at its zero header, before the gzip decoder necessarily reads
    // the checksum/size trailer. Finish the member before trusting its files.
    // https://docs.rs/tar/latest/tar/struct.Archive.html#method.set_ignore_zeros
    std::io::copy(&mut archive.into_inner(), &mut std::io::sink())
        .with_context(|| format!("verifying gzip stream in {}", from.display()))?;
    info!(from = %from.display(), into = %staging.display(), "tarball extracted into staging");

    let placement = if !staging.join(CONFIG_FILE).is_file() {
        ConfigPlacement::Absent
    } else if with_config || !data_dir.join(CONFIG_FILE).exists() {
        ConfigPlacement::Install
    } else {
        ConfigPlacement::Divert
    };

    // 2. Open + drop the staged store so refinery applies any pending
    //    migrations and the SQLite file is validated — still before anything
    //    live is touched. Dropping the store joins the writer thread and
    //    closes every connection, so the directory can be renamed afterwards
    //    on Windows as well.
    let store =
        Store::open_snapshot(staging).context("opening restored store from its snapshot")?;
    let has_api_credentials = store.reader.api_credentials_exist().await?;
    let has_pepper_verifier = store
        .reader
        .api_credential_pepper_verifier()
        .await?
        .is_some();
    if has_api_credentials && !has_pepper_verifier {
        if !accept_legacy_api_key_pepper {
            bail!(
                "the backup contains API keys but predates token-pepper verification; restore the original [auth].token_pepper and rerun with --accept-legacy-api-key-pepper to confirm it (the live data was not changed)"
            );
        }
        let pepper = token_pepper
            .filter(|value| !value.trim().is_empty())
            .context(
                "--accept-legacy-api-key-pepper requires the original non-blank [auth].token_pepper",
            )?;
        let verifier = ai_memory_store::password::hash_password(pepper.to_owned())
            .await
            .context("binding the confirmed legacy API-key pepper")?;
        if !store
            .writer
            .bind_api_credential_pepper_verifier(verifier)
            .await?
        {
            bail!("the restored API-key pepper verifier could not be bound");
        }
    }
    // Keeping the live config must not leave a server that refuses to start:
    // its loaded pepper must match the restored API keys.
    if placement != ConfigPlacement::Install && has_api_credentials {
        let matches = match (
            token_pepper.filter(|value| !value.trim().is_empty()),
            store.reader.api_credential_pepper_verifier().await?,
        ) {
            (Some(pepper), Some(verifier)) => {
                ai_memory_store::password::verify_password(pepper.to_owned(), verifier)
                    .await
                    .context("verifying the live API-key pepper against the backup")?
            }
            _ => false,
        };
        if !matches {
            if placement == ConfigPlacement::Absent {
                bail!(
                    "the backup does not contain config.toml and the configured \
                     [auth].token_pepper is missing or does not match its API keys; restore \
                     the original config or token_pepper before retrying (the live data \
                     was not changed)"
                );
            }
            bail!(
                "the backup's API keys were hashed with a different [auth].token_pepper than \
                 the live config's; rerun with --with-config to restore the backup's \
                 config.toml, or set the live config's token_pepper to the original (the live \
                 data was not changed)"
            );
        }
    }
    drop(store);

    // 3. Exchange the live directories for the staged ones.
    swap_into_place(
        i18n,
        data_dir,
        staging,
        previous,
        placement == ConfigPlacement::Install,
    )?;
    info!(into = %data_dir.display(), "restored data swapped into place");

    Ok(match placement {
        ConfigPlacement::Absent => ConfigOutcome::NotInArchive,
        ConfigPlacement::Install => ConfigOutcome::Restored,
        ConfigPlacement::Divert => {
            let beside = data_dir.join(CONFIG_FROM_BACKUP_FILE);
            save_archived_config(&staging.join(CONFIG_FILE), &beside).with_context(|| {
                format!(
                    "the restore completed and kept the live config.toml, but saving the \
                     backup's config.toml to {} failed",
                    beside.display()
                )
            })?;
            ConfigOutcome::SavedBeside(beside)
        }
    })
}

/// Move the staged config beside the live one, owner-only: it can hold the
/// API-key pepper and other credentials.
fn save_archived_config(staged: &Path, beside: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(staged, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(staged, beside)
}

/// Move the live entries aside into `previous`, move the staged entries
/// into place, then discard `previous`. Every step is a same-filesystem
/// rename; if one fails, the moves already made are reversed so the data
/// dir ends up as it started.
fn swap_into_place(
    i18n: &ai_memory_core::i18n::Localizer,
    data_dir: &Path,
    staging: &Path,
    previous: &Path,
    install_config: bool,
) -> Result<()> {
    std::fs::create_dir(previous).with_context(|| format!("creating {}", previous.display()))?;

    // Entries moved from the data dir into `previous`, and staged entries
    // already placed live: the two lists a rollback has to undo.
    let mut moved_aside: Vec<&str> = Vec::new();
    let mut placed: Vec<&str> = Vec::new();

    let exchange = (|| -> Result<()> {
        for name in REPLACED_DIRS {
            let live = data_dir.join(name);
            if live.exists() {
                std::fs::rename(&live, previous.join(name))
                    .with_context(|| format!("moving {} aside", live.display()))?;
                moved_aside.push(name);
            }
        }
        let live_config = data_dir.join(CONFIG_FILE);
        if install_config && live_config.exists() {
            std::fs::rename(&live_config, previous.join(CONFIG_FILE))
                .with_context(|| format!("moving {} aside", live_config.display()))?;
            moved_aside.push(CONFIG_FILE);
        }
        let config = install_config.then_some(&CONFIG_FILE);
        for name in REPLACED_DIRS.iter().chain(config) {
            let staged = staging.join(name);
            if staged.exists() {
                std::fs::rename(&staged, data_dir.join(name))
                    .with_context(|| format!("moving {} into place", staged.display()))?;
                placed.push(name);
            }
        }
        Ok(())
    })();

    if let Err(e) = exchange {
        if let Err(rollback_err) = roll_back(data_dir, previous, &placed, &moved_aside) {
            bail!(
                "INCONSISTENT STATE: restore swap failed ({e:#}) and moving the previous data \
                 back also failed ({rollback_err:#}); the pre-restore wiki/ and db/ are under {} \
                 — move them back into {} by hand",
                previous.display(),
                data_dir.display(),
            );
        }
        return Err(e.context("restore swap failed; the previous data was moved back into place"));
    }

    // The previous data is only discarded once the restored copy is live —
    // the documented `--force` semantics, now applied last instead of first.
    if let Err(e) = std::fs::remove_dir_all(previous) {
        warn!(path = %previous.display(), error = %e, "could not remove pre-restore data");
        eprintln!(
            "{}",
            i18n.diagnostic(&format!(
                "warning: restore succeeded but the pre-restore data under {} could not be \
             removed ({e}); delete it by hand",
                previous.display()
            ))
        );
    }
    Ok(())
}

/// Undo a partial swap: remove whatever staged entries were already placed
/// live, then move the previous entries back. Placed entries are removed
/// before their predecessors return so a rename never finds its target
/// occupied.
fn roll_back(
    data_dir: &Path,
    previous: &Path,
    placed: &[&str],
    moved_aside: &[&str],
) -> Result<()> {
    for name in placed {
        let live = data_dir.join(name);
        remove_path(&live).with_context(|| format!("removing half-placed {}", live.display()))?;
    }
    for name in moved_aside {
        let parked = previous.join(name);
        std::fs::rename(&parked, data_dir.join(name))
            .with_context(|| format!("moving {} back", parked.display()))?;
    }
    Ok(())
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn unpack_checked_archive<R: std::io::Read>(
    archive: &mut tar::Archive<R>,
    data_dir: &Path,
) -> Result<()> {
    archive.set_preserve_permissions(false);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let entry_type = entry.header().entry_type();
        validate_restore_entry(&path, entry_type)?;
        entry
            .unpack_in(data_dir)
            .with_context(|| format!("extracting {}", path.display()))?;
    }
    Ok(())
}

/// The backup writes `db/memory.sqlite` via `tar::Builder`'s default sparse
/// detection: on a sparse SQLite snapshot the entry type is GNU-sparse
/// (header byte `S`), not plain `Regular` — `tar::EntryType::is_file()` is
/// `false` for it even though it is a hole-encoded regular file. `tar`
/// already expands the sparse blocks to their full logical content while
/// iterating entries (see `Archive::parse_sparse_header`), so `unpack_in`
/// on such an entry writes byte-identical content to a non-sparse one.
fn is_regular_file(entry_type: tar::EntryType) -> bool {
    entry_type.is_file() || entry_type == tar::EntryType::GNUSparse
}

fn validate_restore_entry(path: &Path, entry_type: tar::EntryType) -> Result<()> {
    if !path.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("backup contains unsafe path: {}", path.display());
    }
    if entry_type.is_symlink() || entry_type.is_hard_link() {
        bail!("backup contains unsupported link entry: {}", path.display());
    }
    if !(is_regular_file(entry_type) || entry_type.is_dir()) {
        bail!("backup contains unsupported entry type: {}", path.display());
    }
    let path_str = path.to_string_lossy();
    let allowed = if entry_type.is_dir() {
        path_str == "wiki" || path_str.starts_with("wiki/") || path_str == "db"
    } else {
        path_str == "config.toml" || path_str == "db/memory.sqlite" || path_str.starts_with("wiki/")
    };
    if !allowed {
        bail!("backup contains unexpected path: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{ApiCredentialId, NewUser, UserRole};

    fn archive_with_entry(path: &str, entry_type: tar::EntryType) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            let mut header = tar::Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_entry_type(entry_type);
            if entry_type.is_symlink() || entry_type.is_hard_link() {
                header.set_link_name("/etc/passwd").unwrap();
            }
            let body: &[u8] = if entry_type.is_file() { b"body" } else { b"" };
            header.set_size(body.len() as u64);
            header.set_cksum();
            builder.append(&header, body).unwrap();
            builder.finish().unwrap();
        }
        bytes
    }

    #[test]
    fn restore_accepts_expected_backup_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            builder.append_dir("wiki", tmp.path()).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_path("wiki/default/project/notes/x.md").unwrap();
            header.set_size(4);
            header.set_cksum();
            builder.append(&header, &b"body"[..]).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_path("db/memory.sqlite").unwrap();
            header.set_size(0);
            header.set_cksum();
            builder.append(&header, &b""[..]).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_path("config.toml").unwrap();
            header.set_size(0);
            header.set_cksum();
            builder.append(&header, &b""[..]).unwrap();
            builder.finish().unwrap();
        }

        let restore_dir = tempfile::TempDir::new().unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        unpack_checked_archive(&mut archive, restore_dir.path()).unwrap();
        assert!(
            restore_dir
                .path()
                .join("wiki/default/project/notes/x.md")
                .is_file()
        );
        assert!(restore_dir.path().join("db/memory.sqlite").is_file());
        assert!(restore_dir.path().join("config.toml").is_file());
    }

    #[test]
    fn restore_rejects_link_entries() {
        for entry_type in [tar::EntryType::symlink(), tar::EntryType::hard_link()] {
            let bytes = archive_with_entry("wiki/link.md", entry_type);
            let restore_dir = tempfile::TempDir::new().unwrap();
            let mut archive = tar::Archive::new(bytes.as_slice());
            let err = unpack_checked_archive(&mut archive, restore_dir.path()).unwrap_err();
            assert!(err.to_string().contains("unsupported link entry"));
        }
    }

    #[test]
    fn validate_restore_entry_accepts_gnu_sparse_for_the_sqlite_path() {
        validate_restore_entry(Path::new("db/memory.sqlite"), tar::EntryType::GNUSparse)
            .expect("a GNU-sparse db/memory.sqlite entry is a regular file, just hole-encoded");
    }

    #[test]
    fn validate_restore_entry_still_rejects_gnu_sparse_at_an_unexpected_path() {
        let err =
            validate_restore_entry(Path::new("secret.txt"), tar::EntryType::GNUSparse).unwrap_err();
        assert!(
            err.to_string().contains("unexpected path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn restore_rejects_unexpected_or_unsafe_paths() {
        for path in ["../config.toml", "/tmp/x"] {
            let err = validate_restore_entry(Path::new(path), tar::EntryType::file()).unwrap_err();
            assert!(
                err.to_string().contains("unsafe path"),
                "unexpected error for {path}: {err}"
            );
        }

        let path = "db/extra.sqlite";
        let bytes = archive_with_entry(path, tar::EntryType::file());
        let restore_dir = tempfile::TempDir::new().unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        let err = unpack_checked_archive(&mut archive, restore_dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("unexpected path"),
            "unexpected error for {path}: {err}"
        );
    }

    #[cfg(target_os = "linux")]
    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    /// Whether the `tar` on PATH is GNU tar with sparse support, so the
    /// real-fixture test below can be skipped everywhere else (musl/BSD
    /// `tar`, or no `tar` at all) without breaking those environments.
    #[cfg(target_os = "linux")]
    fn gnu_tar_with_sparse_available() -> bool {
        std::process::Command::new("tar")
            .arg("--version")
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("GNU tar"))
    }

    /// Reproduces #718: the backup command's `tar::Builder` (default
    /// `sparse: true`) writes `db/memory.sqlite` as a GNU-sparse entry
    /// whenever the SQLite snapshot has real holes on disk. This builds an
    /// actual sparse file, archives it with real GNU tar (never a hand-built
    /// header — that would prove nothing about whether unpacking works), and
    /// asserts the restored file is byte-identical to the original logical
    /// content.
    #[cfg(target_os = "linux")]
    #[test]
    fn restore_round_trips_a_real_gnu_sparse_sqlite_snapshot() {
        if !gnu_tar_with_sparse_available() {
            eprintln!(
                "skipping restore_round_trips_a_real_gnu_sparse_sqlite_snapshot: \
                 GNU tar with --sparse not available on PATH"
            );
            return;
        }

        use std::io::{Seek, SeekFrom, Write};

        let src = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(src.path().join("wiki/default/project/notes")).unwrap();
        std::fs::write(src.path().join("wiki/default/project/notes/a.md"), b"body").unwrap();
        std::fs::write(src.path().join("config.toml"), b"# cfg\n").unwrap();
        std::fs::create_dir_all(src.path().join("db")).unwrap();
        let sqlite_path = src.path().join("db/memory.sqlite");
        {
            let mut f = std::fs::File::create(&sqlite_path).unwrap();
            f.write_all(&[0xAB; 8192]).unwrap();
            // Seek far past the last write without writing the gap: on a
            // filesystem that supports holes (tmpfs, ext4, xfs, btrfs) this
            // leaves a real hole rather than allocated zero pages.
            f.seek(SeekFrom::Start(8 * 1024 * 1024)).unwrap();
            f.write_all(&[0xCD; 8192]).unwrap();
        }
        let expected_hash = sha256_hex(&std::fs::read(&sqlite_path).unwrap());

        let archive_dir = tempfile::TempDir::new().unwrap();
        let archive_path = archive_dir.path().join("backup.tar.gz");
        // Distributions may default to pax (`--format=posix`), which encodes
        // sparse files without the GNU-sparse entry type this test needs.
        let status = std::process::Command::new("tar")
            .arg("--format=gnu")
            .arg("--sparse")
            .arg("-czf")
            .arg(&archive_path)
            .arg("-C")
            .arg(src.path())
            .arg("config.toml")
            .arg("wiki")
            .arg("db/memory.sqlite")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "GNU tar failed to build the fixture archive"
        );

        // Confirm the fixture actually reproduces the bug before trusting the
        // restore result: db/memory.sqlite must be a GNU-sparse entry, not a
        // plain regular file (which would exercise nothing new).
        {
            let file = std::fs::File::open(&archive_path).unwrap();
            let dec = GzDecoder::new(file);
            let mut ar = tar::Archive::new(dec);
            let mut found_sparse = false;
            for entry in ar.entries().unwrap() {
                let entry = entry.unwrap();
                if entry.path().unwrap().as_ref() == Path::new("db/memory.sqlite") {
                    found_sparse = entry.header().entry_type() == tar::EntryType::GNUSparse;
                }
            }
            assert!(
                found_sparse,
                "fixture archive did not encode db/memory.sqlite as GNU-sparse; \
                 test does not exercise #718"
            );
        }

        let restore_dir = tempfile::TempDir::new().unwrap();
        let file = std::fs::File::open(&archive_path).unwrap();
        let dec = GzDecoder::new(file);
        let mut ar = tar::Archive::new(dec);
        unpack_checked_archive(&mut ar, restore_dir.path()).unwrap();

        let restored = std::fs::read(restore_dir.path().join("db/memory.sqlite")).unwrap();
        assert_eq!(
            sha256_hex(&restored),
            expected_hash,
            "restored SQLite content must byte-match the original logical content"
        );
        assert!(restore_dir.path().join("config.toml").is_file());
        assert!(
            restore_dir
                .path()
                .join("wiki/default/project/notes/a.md")
                .is_file()
        );
    }

    // ---- stage → validate → swap: a failed restore leaves the live data alone ----

    /// A populated data dir as an operator has it: a wiki page, a database
    /// file (never opened by these tests, so any bytes do), the config, and
    /// the neighbours `logs/` and `raw/` that a restore must never touch.
    fn seed_live_data(dir: &Path) {
        std::fs::create_dir_all(dir.join("wiki/default/project/notes")).unwrap();
        std::fs::write(dir.join("wiki/default/project/notes/old.md"), b"old page").unwrap();
        std::fs::create_dir_all(dir.join("db")).unwrap();
        std::fs::write(dir.join("db/memory.sqlite"), b"old database bytes").unwrap();
        std::fs::write(dir.join("config.toml"), b"# old config\n").unwrap();
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::write(dir.join("logs/app.log"), b"log").unwrap();
        std::fs::create_dir_all(dir.join("raw")).unwrap();
        std::fs::write(dir.join("raw/segment.jsonl"), b"{}").unwrap();
    }

    /// Scratch directories a restore leaves behind only when its cleanup
    /// failed: none may survive a run, successful or not.
    fn restore_scratch_dirs(data_dir: &Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(data_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(".restore-"))
            })
            .collect()
    }

    fn assert_live_data_untouched(dir: &Path) {
        assert_eq!(
            std::fs::read(dir.join("wiki/default/project/notes/old.md")).unwrap(),
            b"old page"
        );
        assert_eq!(
            std::fs::read(dir.join("db/memory.sqlite")).unwrap(),
            b"old database bytes"
        );
        assert_eq!(
            std::fs::read(dir.join("config.toml")).unwrap(),
            b"# old config\n"
        );
        assert_eq!(std::fs::read(dir.join("logs/app.log")).unwrap(), b"log");
        assert_eq!(std::fs::read(dir.join("raw/segment.jsonl")).unwrap(), b"{}");
        let scratch = restore_scratch_dirs(dir);
        assert!(scratch.is_empty(), "scratch dirs left behind: {scratch:?}");
    }

    /// Bytes of a real, migrated `memory.sqlite` — what a `backup` tarball
    /// carries.
    fn migrated_sqlite_bytes() -> Vec<u8> {
        let tmp = tempfile::TempDir::new().unwrap();
        drop(Store::open(tmp.path()).unwrap());
        std::fs::read(tmp.path().join("db/memory.sqlite")).unwrap()
    }

    /// A snapshot holding one API key, with the pepper verifier bound to
    /// `pepper`, or none (a pre-verifier backup) when it is `None`.
    async fn api_key_sqlite_bytes(pepper: Option<&str>) -> Vec<u8> {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let user_id = store
            .writer
            .create_human_user(
                NewUser {
                    username: "legacy-restore-user".into(),
                    name: None,
                    email: None,
                },
                UserRole::User,
                None,
                false,
            )
            .await
            .unwrap();
        store
            .writer
            .create_api_credential(
                ApiCredentialId::new(),
                user_id,
                "legacy-key".into(),
                [0xA5; 32],
                Some("...test".into()),
            )
            .await
            .unwrap();
        assert!(
            store
                .reader
                .api_credential_pepper_verifier()
                .await
                .unwrap()
                .is_none()
        );
        if let Some(pepper) = pepper {
            let verifier = ai_memory_store::password::hash_password(pepper.to_owned())
                .await
                .unwrap();
            assert!(
                store
                    .writer
                    .bind_api_credential_pepper_verifier(verifier)
                    .await
                    .unwrap()
            );
        }
        let snapshot = tmp.path().join("legacy-snapshot.sqlite");
        store.reader.snapshot_to(snapshot.clone()).await.unwrap();
        drop(store);
        std::fs::read(snapshot).unwrap()
    }

    /// A gzipped tarball holding the given regular files.
    fn gz_tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write as _;

        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for (path, body) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append(&header, *body).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        gz.finish().unwrap()
    }

    fn good_archive() -> Vec<u8> {
        let db = migrated_sqlite_bytes();
        gz_tarball(&[
            ("wiki/default/project/notes/new.md", b"new page"),
            ("db/memory.sqlite", db.as_slice()),
            ("config.toml", b"# restored config\n"),
        ])
    }

    fn write_tarball(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[tokio::test]
    async fn restore_force_keeps_live_data_when_the_tarball_is_not_a_gzip_stream() {
        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let scratch = tempfile::TempDir::new().unwrap();
        let from = write_tarball(scratch.path(), "bad.tar.gz", b"this is not a gzip stream");

        let err = restore_data_dir(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            false,
        )
        .await
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("extracting"),
            "unexpected error: {err:#}"
        );
        assert_live_data_untouched(data.path());
    }

    #[tokio::test]
    async fn restore_force_keeps_live_data_when_the_tarball_is_truncated() {
        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let scratch = tempfile::TempDir::new().unwrap();
        let whole = good_archive();
        let from = write_tarball(scratch.path(), "cut.tar.gz", &whole[..whole.len() / 2]);

        let err = restore_data_dir(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            false,
        )
        .await
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("extracting"),
            "unexpected error: {err:#}"
        );
        assert_live_data_untouched(data.path());
    }

    #[tokio::test]
    async fn restore_force_keeps_live_data_when_the_gzip_checksum_is_invalid() {
        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let scratch = tempfile::TempDir::new().unwrap();
        let mut bytes = good_archive();
        // All tar entries are readable, but the gzip trailer's CRC32 is wrong.
        let checksum_offset = bytes.len() - 8;
        bytes[checksum_offset] ^= 0x80;
        let from = write_tarball(scratch.path(), "bad-crc.tar.gz", &bytes);

        let err = restore_data_dir(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            false,
        )
        .await
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("verifying gzip stream"),
            "unexpected error: {err:#}"
        );
        assert_live_data_untouched(data.path());
    }

    #[tokio::test]
    async fn restore_force_keeps_live_data_when_the_gzip_trailer_is_missing() {
        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let scratch = tempfile::TempDir::new().unwrap();
        let bytes = good_archive();
        // Keep the compressed tar, including its end marker, but remove the
        // gzip checksum and original-size trailer.
        let from = write_tarball(
            scratch.path(),
            "no-trailer.tar.gz",
            &bytes[..bytes.len() - 8],
        );

        let err = restore_data_dir(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            false,
        )
        .await
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("verifying gzip stream"),
            "unexpected error: {err:#}"
        );
        assert_live_data_untouched(data.path());
    }

    #[tokio::test]
    async fn restore_force_keeps_live_data_when_an_entry_is_outside_the_allowed_layout() {
        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let scratch = tempfile::TempDir::new().unwrap();
        // Valid entries first, so extraction is already under way when the
        // stray one is met — the failure must still leave nothing behind.
        let db = migrated_sqlite_bytes();
        let from = write_tarball(
            scratch.path(),
            "stray.tar.gz",
            &gz_tarball(&[
                ("wiki/default/project/notes/new.md", b"new page"),
                ("db/memory.sqlite", db.as_slice()),
                ("db/extra.sqlite", b"stray"),
            ]),
        );

        let err = restore_data_dir(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            false,
        )
        .await
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("unexpected path"),
            "unexpected error: {err:#}"
        );
        assert_live_data_untouched(data.path());
    }

    #[tokio::test]
    async fn restore_force_keeps_live_data_when_the_restored_store_cannot_open() {
        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let scratch = tempfile::TempDir::new().unwrap();
        let from = write_tarball(
            scratch.path(),
            "notadb.tar.gz",
            &gz_tarball(&[
                ("wiki/default/project/notes/new.md", b"new page"),
                ("db/memory.sqlite", &[0xFF; 4096]),
            ]),
        );

        let err = restore_data_dir(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            false,
        )
        .await
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("opening restored store"),
            "unexpected error: {err:#}"
        );
        assert_live_data_untouched(data.path());
    }

    #[tokio::test]
    async fn restore_force_keeps_live_data_when_the_snapshot_is_missing() {
        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let scratch = tempfile::TempDir::new().unwrap();
        let from = write_tarball(
            scratch.path(),
            "missing-db.tar.gz",
            &gz_tarball(&[("wiki/default/project/notes/new.md", b"new page")]),
        );

        let error = restore_data_dir(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            false,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("initialized ai-memory database snapshot"));
        assert_live_data_untouched(data.path());
    }

    #[tokio::test]
    async fn restore_force_keeps_live_data_when_the_snapshot_is_zero_bytes() {
        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let scratch = tempfile::TempDir::new().unwrap();
        let from = write_tarball(
            scratch.path(),
            "empty-db.tar.gz",
            &gz_tarball(&[
                ("wiki/default/project/notes/new.md", b"new page"),
                ("db/memory.sqlite", b""),
            ]),
        );

        let error = restore_data_dir(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            false,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("initialized ai-memory database snapshot"));
        assert_live_data_untouched(data.path());
    }

    #[tokio::test]
    async fn restore_force_keeps_live_data_when_the_snapshot_has_no_schema() {
        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let scratch = tempfile::TempDir::new().unwrap();
        let snapshot = scratch.path().join("blank.sqlite");
        let conn = rusqlite::Connection::open(&snapshot).unwrap();
        conn.execute_batch("VACUUM").unwrap();
        drop(conn);
        let db = std::fs::read(&snapshot).unwrap();
        assert!(
            !db.is_empty(),
            "the fixture must be a non-empty SQLite file"
        );
        let from = write_tarball(
            scratch.path(),
            "uninitialized-db.tar.gz",
            &gz_tarball(&[("db/memory.sqlite", &db)]),
        );

        let error = restore_data_dir(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            false,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("initialized ai-memory database snapshot"));
        assert_live_data_untouched(data.path());
    }

    #[tokio::test]
    async fn restore_accepts_an_initialized_snapshot_without_pages() {
        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let scratch = tempfile::TempDir::new().unwrap();
        let db = migrated_sqlite_bytes();
        let from = write_tarball(
            scratch.path(),
            "empty-store.tar.gz",
            &gz_tarball(&[("db/memory.sqlite", &db)]),
        );

        restore_data_dir(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            false,
        )
        .await
        .unwrap();

        let conn = rusqlite::Connection::open(data.path().join("db/memory.sqlite")).unwrap();
        let pages: i64 = conn
            .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(pages, 0);
        assert!(!data.path().join("wiki").exists());
        assert_eq!(
            std::fs::read(data.path().join("config.toml")).unwrap(),
            b"# old config\n"
        );
        assert!(restore_scratch_dirs(data.path()).is_empty());
    }

    #[tokio::test]
    async fn restore_refuses_a_populated_data_dir_without_force() {
        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let scratch = tempfile::TempDir::new().unwrap();
        let from = write_tarball(scratch.path(), "good.tar.gz", &good_archive());

        let err = restore_data_dir(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            false,
            false,
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string().contains("pass --force"),
            "unexpected error: {err:#}"
        );
        assert_live_data_untouched(data.path());
    }

    #[tokio::test]
    async fn restore_force_with_config_replaces_the_live_data_and_config() {
        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let scratch = tempfile::TempDir::new().unwrap();
        let from = write_tarball(scratch.path(), "good.tar.gz", &good_archive());

        let outcome = restore_data_dir(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            true,
        )
        .await
        .unwrap();
        assert_eq!(outcome, ConfigOutcome::Restored);
        assert!(!data.path().join(CONFIG_FROM_BACKUP_FILE).exists());

        assert_eq!(
            std::fs::read(data.path().join("wiki/default/project/notes/new.md")).unwrap(),
            b"new page"
        );
        assert!(
            !data
                .path()
                .join("wiki/default/project/notes/old.md")
                .exists(),
            "the previous wiki must not be merged into the restored one"
        );
        assert_eq!(
            std::fs::read(data.path().join("config.toml")).unwrap(),
            b"# restored config\n"
        );
        assert_eq!(
            std::fs::read(data.path().join("logs/app.log")).unwrap(),
            b"log"
        );
        assert_eq!(
            std::fs::read(data.path().join("raw/segment.jsonl")).unwrap(),
            b"{}"
        );
        let scratch_dirs = restore_scratch_dirs(data.path());
        assert!(
            scratch_dirs.is_empty(),
            "scratch dirs left behind: {scratch_dirs:?}"
        );
        // The swapped-in store is the migrated one and opens cleanly in place.
        drop(Store::open(data.path()).unwrap());
    }

    /// A backup's config can run commands, so by default the live one stays
    /// and the archived one is saved beside it, owner-only, for review.
    #[tokio::test]
    async fn restore_keeps_a_live_config_and_saves_the_archived_one_beside_it() {
        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let scratch = tempfile::TempDir::new().unwrap();
        let archived = b"[auto_improve.eval]\nenabled = true\ncommand = [\"/bin/false\"]\n";
        let db = migrated_sqlite_bytes();
        let from = write_tarball(
            scratch.path(),
            "code.tar.gz",
            &gz_tarball(&[
                ("wiki/default/project/notes/new.md", b"new page"),
                ("db/memory.sqlite", db.as_slice()),
                ("config.toml", archived),
            ]),
        );

        let outcome = restore_data_dir(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            false,
        )
        .await
        .unwrap();

        let beside = data.path().join(CONFIG_FROM_BACKUP_FILE);
        assert_eq!(outcome, ConfigOutcome::SavedBeside(beside.clone()));
        assert_eq!(
            std::fs::read(data.path().join("config.toml")).unwrap(),
            b"# old config\n",
            "the live config must not be replaced without --with-config"
        );
        assert_eq!(std::fs::read(&beside).unwrap(), archived);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&beside).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the archived config can hold credentials");
        }
        // The data itself is restored.
        assert_eq!(
            std::fs::read(data.path().join("wiki/default/project/notes/new.md")).unwrap(),
            b"new page"
        );
        assert!(restore_scratch_dirs(data.path()).is_empty());
    }

    #[tokio::test]
    async fn restore_keeps_the_live_config_when_the_archive_carries_none() {
        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let scratch = tempfile::TempDir::new().unwrap();
        let db = migrated_sqlite_bytes();
        let from = write_tarball(
            scratch.path(),
            "noconfig.tar.gz",
            &gz_tarball(&[
                ("wiki/default/project/notes/new.md", b"new page"),
                ("db/memory.sqlite", db.as_slice()),
            ]),
        );

        let outcome = restore_data_dir(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            true,
        )
        .await
        .unwrap();

        assert_eq!(outcome, ConfigOutcome::NotInArchive);
        assert!(!data.path().join(CONFIG_FROM_BACKUP_FILE).exists());
        assert_eq!(
            std::fs::read(data.path().join("config.toml")).unwrap(),
            b"# old config\n"
        );
        assert_eq!(
            std::fs::read(data.path().join("wiki/default/project/notes/new.md")).unwrap(),
            b"new page"
        );
        assert!(restore_scratch_dirs(data.path()).is_empty());
    }

    #[tokio::test]
    async fn restore_into_an_empty_data_dir_needs_no_force() {
        let data = tempfile::TempDir::new().unwrap();
        let scratch = tempfile::TempDir::new().unwrap();
        let from = write_tarball(scratch.path(), "good.tar.gz", &good_archive());

        let outcome = restore_data_dir(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            false,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read(data.path().join("wiki/default/project/notes/new.md")).unwrap(),
            b"new page"
        );
        assert!(data.path().join("db/memory.sqlite").is_file());
        // With no live config to keep, the archived one is restored in place.
        assert_eq!(outcome, ConfigOutcome::Restored);
        assert_eq!(
            std::fs::read(data.path().join("config.toml")).unwrap(),
            b"# restored config\n"
        );
        assert!(!data.path().join(CONFIG_FROM_BACKUP_FILE).exists());
        assert!(restore_scratch_dirs(data.path()).is_empty());
    }

    #[tokio::test]
    async fn legacy_api_key_restore_requires_original_pepper_confirmation() {
        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let scratch = tempfile::TempDir::new().unwrap();
        let db = api_key_sqlite_bytes(None).await;
        let from = write_tarball(
            scratch.path(),
            "legacy.tar.gz",
            &gz_tarball(&[("db/memory.sqlite", db.as_slice())]),
        );

        let refused = restore_data_dir_with_legacy_pepper(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            false,
            Some("original-pepper"),
            false,
        )
        .await
        .unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("--accept-legacy-api-key-pepper"),
            "unexpected error: {refused:#}"
        );
        assert_live_data_untouched(data.path());

        restore_data_dir_with_legacy_pepper(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            false,
            Some("original-pepper"),
            true,
        )
        .await
        .unwrap();
        let restored = Store::open(data.path()).unwrap();
        let verifier = restored
            .reader
            .api_credential_pepper_verifier()
            .await
            .unwrap()
            .expect("confirmed pepper must be sealed into the restored database");
        assert!(
            ai_memory_store::password::verify_password("original-pepper".into(), verifier)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn restoring_without_archived_config_checks_the_live_pepper_before_swap() {
        let scratch = tempfile::TempDir::new().unwrap();
        let db = api_key_sqlite_bytes(Some("archive-pepper")).await;
        let from = write_tarball(
            scratch.path(),
            "keys-without-config.tar.gz",
            &gz_tarball(&[
                ("wiki/default/project/notes/new.md", b"new page"),
                ("db/memory.sqlite", db.as_slice()),
            ]),
        );

        // The flag cannot supply a config the archive does not contain.
        for with_config in [false, true] {
            for live_pepper in [Some("different-live-pepper"), None, Some(" \t")] {
                let data = tempfile::TempDir::new().unwrap();
                seed_live_data(data.path());
                let err = restore_data_dir_with_legacy_pepper(
                    &ai_memory_core::i18n::Localizer::default(),
                    data.path(),
                    &from,
                    true,
                    with_config,
                    live_pepper,
                    false,
                )
                .await
                .unwrap_err();
                let message = err.to_string();
                assert!(message.contains("does not contain config.toml"), "{err:#}");
                assert!(message.contains("[auth].token_pepper"), "{err:#}");
                assert!(!message.contains("--with-config"), "{err:#}");
                assert_live_data_untouched(data.path());
                assert!(!data.path().join(CONFIG_FROM_BACKUP_FILE).exists());
            }

            let data = tempfile::TempDir::new().unwrap();
            seed_live_data(data.path());
            let outcome = restore_data_dir_with_legacy_pepper(
                &ai_memory_core::i18n::Localizer::default(),
                data.path(),
                &from,
                true,
                with_config,
                Some("archive-pepper"),
                false,
            )
            .await
            .unwrap();
            assert_eq!(outcome, ConfigOutcome::NotInArchive);
            assert_eq!(
                std::fs::read(data.path().join("config.toml")).unwrap(),
                b"# old config\n"
            );
            assert_eq!(
                std::fs::read(data.path().join("wiki/default/project/notes/new.md")).unwrap(),
                b"new page"
            );
            assert!(!data.path().join(CONFIG_FROM_BACKUP_FILE).exists());
            assert!(restore_scratch_dirs(data.path()).is_empty());
            let restored = Store::open(data.path()).unwrap();
            assert!(restored.reader.api_credentials_exist().await.unwrap());
        }
    }

    /// Keeping the live config must not leave a server that refuses to start
    /// on the restored API keys. A pepper mismatch is refused before anything
    /// changes and names `--with-config`; with the flag, or with the
    /// original pepper live, the restore goes through.
    #[tokio::test]
    async fn keeping_the_live_config_requires_its_pepper_to_match_the_restored_keys() {
        let scratch = tempfile::TempDir::new().unwrap();
        let db = api_key_sqlite_bytes(Some("archive-pepper")).await;
        let archived = b"[auth]\ntoken_pepper = \"archive-pepper\"\n";
        let from = write_tarball(
            scratch.path(),
            "keys.tar.gz",
            &gz_tarball(&[
                ("wiki/default/project/notes/new.md", b"new page"),
                ("db/memory.sqlite", db.as_slice()),
                ("config.toml", archived),
            ]),
        );

        for live_pepper in [Some("live-pepper"), None] {
            let data = tempfile::TempDir::new().unwrap();
            seed_live_data(data.path());
            let refused = restore_data_dir_with_legacy_pepper(
                &ai_memory_core::i18n::Localizer::default(),
                data.path(),
                &from,
                true,
                false,
                live_pepper,
                false,
            )
            .await
            .unwrap_err();
            assert!(
                refused.to_string().contains("--with-config"),
                "unexpected error: {refused:#}"
            );
            assert_live_data_untouched(data.path());
            assert!(!data.path().join(CONFIG_FROM_BACKUP_FILE).exists());
        }

        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let outcome = restore_data_dir_with_legacy_pepper(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            true,
            Some("live-pepper"),
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome, ConfigOutcome::Restored);
        assert_eq!(
            std::fs::read(data.path().join("config.toml")).unwrap(),
            archived
        );

        let data = tempfile::TempDir::new().unwrap();
        seed_live_data(data.path());
        let outcome = restore_data_dir_with_legacy_pepper(
            &ai_memory_core::i18n::Localizer::default(),
            data.path(),
            &from,
            true,
            false,
            Some("archive-pepper"),
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            ConfigOutcome::SavedBeside(data.path().join(CONFIG_FROM_BACKUP_FILE))
        );
        assert_eq!(
            std::fs::read(data.path().join("config.toml")).unwrap(),
            b"# old config\n"
        );
    }
}
