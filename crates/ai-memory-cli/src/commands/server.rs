//! `ai-memory server add|list|remove` — manage local server profiles (#992).
//!
//! Local-only: writes `<data_dir>/servers.toml` and `<data_dir>/auth-tokens/`,
//! never contacts a server. See `server_profiles` for the routing rules.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use crate::cli::{ServerAddArgs, ServerArgs, ServerCommand, ServerListArgs, ServerRemoveArgs};
use crate::config::Config;
use crate::server_profiles::{self, ProfileName};

/// Run a `server` subcommand.
///
/// # Errors
/// Returns an error for an invalid name, URL, or root, an invalid existing
/// `servers.toml`, or a failed write.
pub fn run(config: &Config, args: ServerArgs) -> Result<()> {
    let mut stdout = std::io::stdout();
    let root_token = config.auth.token_file_path();
    match &args.command {
        ServerCommand::Add(ServerAddArgs { name, .. })
        | ServerCommand::Remove(ServerRemoveArgs { name }) => {
            refuse_the_root_token_name(&config.data_dir, name, root_token)?;
        }
        ServerCommand::List(_) => {}
    }
    match args.command {
        ServerCommand::Add(args) => {
            let stdin_token = if args.auth_token_stdin {
                Some(read_token_line(&mut std::io::stdin().lock())?)
            } else {
                None
            };
            add(
                &config.i18n,
                &config.data_dir,
                args,
                stdin_token,
                &mut stdout,
            )
        }
        ServerCommand::List(args) => list(&config.i18n, &config.data_dir, &args, &mut stdout),
        ServerCommand::Remove(args) => remove(&config.i18n, &config.data_dir, &args, &mut stdout),
    }
}

fn parse_name(raw: &str) -> Result<ProfileName> {
    ProfileName::parse(raw).ok_or_else(|| {
        anyhow!(
            "`{raw}` is not a valid profile name: use 1-64 lowercase letters, digits, `-` or `_`, \
             starting with a letter or digit"
        )
    })
}

/// `auth-tokens/default` holds this install's root bearer once `init` or
/// `auth enable` provisioned it. A profile of that name would read the same
/// file, so the hook would present the root bearer to that profile's server
/// (and `server add --auth-token` would overwrite the root bearer, `server
/// remove` delete it).
fn refuse_the_root_token_name(data_dir: &Path, raw: &str, root_token: Option<&Path>) -> Result<()> {
    let Some(name) = ProfileName::parse(raw) else {
        return Ok(());
    };
    if server_profiles::profile_shares_root_token(data_dir, &name, root_token) {
        bail!(
            "`{name}` names this server's own root bearer token file ([auth].bearer_token_file); \
             choose another profile name"
        );
    }
    Ok(())
}

fn read_token_line(input: &mut impl std::io::BufRead) -> Result<String> {
    let mut line = String::new();
    input
        .read_line(&mut line)
        .context("reading the token from stdin")?;
    let token = line.trim();
    if token.is_empty() {
        bail!("--auth-token-stdin was given but stdin had no token");
    }
    Ok(token.to_owned())
}

fn add(
    i18n: &ai_memory_core::i18n::Localizer,
    data_dir: &Path,
    args: ServerAddArgs,
    stdin_token: Option<String>,
    out: &mut impl Write,
) -> Result<()> {
    let name = parse_name(&args.name)?;
    let token = stdin_token.or(args.auth_token);
    let outcome = server_profiles::add(data_dir, &name, &args.url, &args.roots, token.as_deref())?;
    writeln!(
        out,
        "{}",
        i18n.diagnostic(&format!("Registered server profile `{name}`."))
    )?;
    if outcome.roots_kept {
        writeln!(
            out,
            "{}",
            i18n.diagnostic(&format!(
                "Kept the roots already registered for `{name}`; pass --root to replace them."
            ))
        )?;
    }
    if outcome.token_discarded {
        writeln!(
            out,
            "{}",
            i18n.diagnostic(
                "The URL changed, so the token stored for the previous URL was removed."
            )
        )?;
    }
    if server_profiles::read_token(data_dir, &name).is_none() {
        writeln!(
            out,
            "{}",
            i18n.diagnostic(&format!(
                "No token is stored for `{name}`: repositories selecting it drop their capture \
             until one is added with `ai-memory server add {name} --url … --auth-token-stdin`."
            ))
        )?;
    }
    let registry = server_profiles::load(data_dir)?;
    let unrooted: Vec<&str> = registry
        .profiles
        .iter()
        .filter(|(_, profile)| profile.roots.is_empty())
        .map(|(name, _)| name.as_str())
        .collect();
    if registry.profiles.len() > 1 && !unrooted.is_empty() {
        writeln!(
            out,
            "{}",
            i18n.diagnostic(&format!(
                "Several profiles are registered, so a profile without --root is refused. \
             Add roots to: {}",
                unrooted.join(", ")
            ))
        )?;
    }
    Ok(())
}

fn list(
    i18n: &ai_memory_core::i18n::Localizer,
    data_dir: &Path,
    args: &ServerListArgs,
    out: &mut impl Write,
) -> Result<()> {
    let registry = server_profiles::load(data_dir)?;
    if args.json {
        let rows: Vec<_> = registry
            .profiles
            .iter()
            .map(|(name, profile)| {
                serde_json::json!({
                    "name": name.as_str(),
                    "url": profile.url,
                    "roots": profile.roots,
                    "token": token_state(data_dir, name),
                })
            })
            .collect();
        writeln!(out, "{}", serde_json::to_string_pretty(&rows)?)?;
        return Ok(());
    }
    if registry.profiles.is_empty() {
        writeln!(out, "{}", i18n.diagnostic("No server profiles registered."))?;
        return Ok(());
    }
    for (name, profile) in &registry.profiles {
        let roots = if profile.roots.is_empty() {
            "(any)".to_owned()
        } else {
            profile.roots.join(", ")
        };
        writeln!(
            out,
            "{}",
            i18n.diagnostic(&format!(
                "{name}\n  url:   {}\n  roots: {roots}\n  token: {}",
                profile.url,
                token_state(data_dir, name)
            ))
        )?;
    }
    Ok(())
}

fn token_state(data_dir: &Path, name: &ProfileName) -> &'static str {
    if server_profiles::read_token(data_dir, name).is_some() {
        "stored"
    } else {
        "missing"
    }
}

fn remove(
    i18n: &ai_memory_core::i18n::Localizer,
    data_dir: &Path,
    args: &ServerRemoveArgs,
    out: &mut impl Write,
) -> Result<()> {
    let name = parse_name(&args.name)?;
    if server_profiles::remove(data_dir, &name)? {
        writeln!(
            out,
            "{}",
            i18n.diagnostic(&format!("Removed server profile `{name}`."))
        )?;
    } else {
        writeln!(
            out,
            "{}",
            i18n.diagnostic(&format!("No server profile named `{name}`."))
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_args(name: &str, url: &str, roots: &[&str], token: Option<&str>) -> ServerAddArgs {
        ServerAddArgs {
            name: name.to_owned(),
            url: url.to_owned(),
            roots: roots.iter().map(|r| (*r).to_owned()).collect(),
            auth_token: token.map(str::to_owned),
            auth_token_stdin: false,
        }
    }

    fn output(run: impl FnOnce(&mut Vec<u8>) -> Result<()>) -> String {
        let mut out = Vec::new();
        run(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// `server add` that must succeed; returns what it printed.
    fn add_ok(data_dir: &Path, args: ServerAddArgs, stdin_token: Option<String>) -> String {
        output(|out| {
            add(
                &ai_memory_core::i18n::Localizer::default(),
                data_dir,
                args,
                stdin_token,
                out,
            )
        })
    }

    /// A profile named after the root bearer's file is refused while that
    /// file is the server's credential; any other name, or the same name on an
    /// install without a root bearer file, is accepted.
    #[test]
    fn the_root_bearer_file_name_cannot_be_registered_or_removed() {
        let dd = tempfile::tempdir().unwrap();
        let root = server_profiles::root_token_path(dd.path());
        let err = refuse_the_root_token_name(dd.path(), "default", Some(&root)).unwrap_err();
        assert!(err.to_string().contains("root bearer"), "{err}");
        assert!(refuse_the_root_token_name(dd.path(), "team-b", Some(&root)).is_ok());
        assert!(refuse_the_root_token_name(dd.path(), "default", None).is_ok());
    }

    /// The listing reports whether a token is stored and never the token.
    #[test]
    fn list_never_prints_a_token() {
        let dd = tempfile::tempdir().unwrap();
        let root = dd.path().join("b").to_string_lossy().into_owned();
        add_ok(
            dd.path(),
            add_args("team-b", "https://b.example", &[&root], Some("SECRET-B")),
            None,
        );
        add_ok(
            dd.path(),
            add_args("team-c", "https://c.example", &[&root], None),
            None,
        );

        for json in [false, true] {
            let listed = output(|out| {
                list(
                    &ai_memory_core::i18n::Localizer::default(),
                    dd.path(),
                    &ServerListArgs { json },
                    out,
                )
            });
            assert!(!listed.contains("SECRET-B"), "{listed}");
            assert!(listed.contains("https://b.example"), "{listed}");
            assert!(listed.contains("stored"), "{listed}");
            assert!(listed.contains("missing"), "{listed}");
        }
    }

    #[test]
    fn a_token_from_stdin_wins_and_is_stored() {
        let dd = tempfile::tempdir().unwrap();
        let token = read_token_line(&mut std::io::Cursor::new("from-stdin\n")).unwrap();
        add_ok(
            dd.path(),
            add_args("a", "https://a.example", &[], None),
            Some(token),
        );
        let name = ProfileName::parse("a").unwrap();
        assert_eq!(
            server_profiles::read_token(dd.path(), &name).as_deref(),
            Some("from-stdin")
        );
        assert!(read_token_line(&mut std::io::Cursor::new("\n")).is_err());
    }

    #[test]
    fn adding_a_second_unrooted_profile_warns_that_it_will_be_refused() {
        let dd = tempfile::tempdir().unwrap();
        add_ok(
            dd.path(),
            add_args("a", "https://a.example", &[], Some("t")),
            None,
        );
        let text = add_ok(
            dd.path(),
            add_args("b", "https://b.example", &[], Some("t")),
            None,
        );
        assert!(text.contains("Add roots to: a, b"), "{text}");
    }

    #[test]
    fn invalid_names_are_refused_before_anything_is_written() {
        let dd = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let bad = add_args("../x", "https://a.example", &[], Some("t"));
        assert!(
            add(
                &ai_memory_core::i18n::Localizer::default(),
                dd.path(),
                bad,
                None,
                &mut out
            )
            .is_err()
        );
        assert!(!dd.path().join("servers.toml").exists());
        let remove_bad = ServerRemoveArgs {
            name: "../x".into(),
        };
        assert!(
            remove(
                &ai_memory_core::i18n::Localizer::default(),
                dd.path(),
                &remove_bad,
                &mut out
            )
            .is_err()
        );
    }

    #[test]
    fn remove_reports_whether_the_profile_existed() {
        let dd = tempfile::tempdir().unwrap();
        add_ok(
            dd.path(),
            add_args("a", "https://a.example", &[], Some("t")),
            None,
        );
        let args = ServerRemoveArgs { name: "a".into() };
        assert!(
            output(|out| remove(
                &ai_memory_core::i18n::Localizer::default(),
                dd.path(),
                &args,
                out
            ))
            .contains("Removed")
        );
        assert!(
            output(|out| remove(
                &ai_memory_core::i18n::Localizer::default(),
                dd.path(),
                &args,
                out
            ))
            .contains("No server profile")
        );
    }
}
