//! `ai-memory user` — manage human users, password login and project grants.
//!
//! Thin HTTP client over `/admin/users*` and `/admin/human-users`. The
//! caller's bearer token must authenticate as root. API keys, including the
//! `legacy-user-token` rows left by the removed 1.x token commands, are
//! managed by `ai-memory api-key`.

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::{
    UserAddHumanArgs, UserArgs, UserCommand, UserDisableArgs, UserEnableArgs, UserListArgs,
    UserPatchArgs, UserResetPasswordArgs,
};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json, patch_json, post_json};

#[derive(Debug, Deserialize, Serialize)]
struct UserRow {
    id: String,
    username: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    created_at: i64,
    #[serde(default)]
    last_seen_at: Option<i64>,
    #[serde(default)]
    role: String,
    #[serde(default)]
    must_change_password: bool,
    #[serde(default)]
    disabled_at: Option<i64>,
    #[serde(default)]
    has_password: bool,
}

impl UserRow {
    fn status(&self) -> &'static str {
        if self.disabled_at.is_some() {
            "disabled"
        } else if self.has_password && self.must_change_password {
            "must-change"
        } else {
            "active"
        }
    }
}

#[derive(Debug, Deserialize)]
struct UserWithPassword {
    user: UserRow,
    temporary_password: String,
}

#[derive(Debug, Deserialize)]
struct UserResponse {
    user: UserRow,
}

#[derive(Debug, Deserialize)]
struct UserList {
    users: Vec<UserRow>,
}

/// Dispatch entry point for `ai-memory user <subcommand>`.
///
/// # Errors
/// Returns an error if the HTTP call fails, the server returns non-2xx,
/// or the response body can't be deserialised.
pub async fn run(config: &Config, args: UserArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    match args.command {
        UserCommand::AddHuman(args) => add_human(&ep, args).await,
        UserCommand::List(args) => list(&ep, args).await,
        UserCommand::ResetPassword(args) => reset_password(&ep, args).await,
        UserCommand::Disable(args) => disable(&ep, args).await,
        UserCommand::Enable(args) => enable(&ep, args).await,
        UserCommand::Patch(args) => patch(&ep, args).await,
        UserCommand::Grant(args) => crate::commands::grant::grant(&ep, &args).await,
        UserCommand::Revoke(args) => crate::commands::grant::revoke(&ep, &args).await,
        UserCommand::Grants(args) => crate::commands::grant::list_for_user(&ep, &args).await,
    }
}

#[derive(Debug, Serialize)]
struct CreateUserBody<'a> {
    username: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'a str>,
}

async fn add_human(ep: &ServerEndpoint, args: UserAddHumanArgs) -> Result<()> {
    let i18n = &ep.i18n;
    let role = args
        .role
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let body = CreateUserBody {
        username: args.username.trim(),
        name: args
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        email: args
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        role,
    };
    let resp: UserWithPassword = post_json(ep, "/admin/human-users", &body)
        .await
        .context(i18n.text("cli-user-creating-human-user"))?;
    print_temp_password(i18n, &resp, args.json, "created")
}

async fn list(ep: &ServerEndpoint, args: UserListArgs) -> Result<()> {
    let i18n = &ep.i18n;
    let resp: UserList = get_json(ep, "/admin/users", &[])
        .await
        .context(i18n.text("cli-user-listing-users"))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp.users)?);
        return Ok(());
    }
    if resp.users.is_empty() {
        println!("{}", i18n.text("cli-user-no-registered-users"));
        return Ok(());
    }
    write_users(&resp.users, &mut io::stdout().lock())
}

/// Lay out the users table using terminal display-cell widths. Usernames and
/// display names are free-form UTF-8, so a byte count over-pads an accented
/// name and misplaces every cell after a double-width one.
fn write_users(users: &[UserRow], out: &mut impl Write) -> Result<()> {
    let user_w = users
        .iter()
        .map(|u| unicode_width::UnicodeWidthStr::width(u.username.as_str()))
        .max()
        .unwrap_or(8)
        .max(8);
    let name_w = users
        .iter()
        .filter_map(|u| u.name.as_deref())
        .map(unicode_width::UnicodeWidthStr::width)
        .max()
        .unwrap_or(4)
        .max(4);
    let role_w = users
        .iter()
        .map(|u| unicode_width::UnicodeWidthStr::width(u.role.as_str()))
        .max()
        .unwrap_or(4)
        .max(4);
    writeln!(
        out,
        "{}  {}  {}  {:<12}",
        pad_cell("USERNAME", user_w),
        pad_cell("NAME", name_w),
        pad_cell("ROLE", role_w),
        "STATUS"
    )?;
    for u in users {
        writeln!(
            out,
            "{}  {}  {}  {:<12}",
            pad_cell(&u.username, user_w),
            pad_cell(u.name.as_deref().unwrap_or("-"), name_w),
            pad_cell(&u.role, role_w),
            u.status()
        )?;
    }
    Ok(())
}

fn pad_cell(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(unicode_width::UnicodeWidthStr::width(value));
    format!("{value}{}", " ".repeat(padding))
}

async fn reset_password(ep: &ServerEndpoint, args: UserResetPasswordArgs) -> Result<()> {
    let i18n = &ep.i18n;
    if !args.yes {
        confirm(&i18n.one("cli-user-confirm-password", &args.username))?;
    }
    let path = format!("/admin/users/{}/reset-password", url_encode(&args.username));
    let resp: UserWithPassword = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context(i18n.text("cli-user-resetting-password"))?;
    print_temp_password(i18n, &resp, args.json, "reset password for")
}

async fn disable(ep: &ServerEndpoint, args: UserDisableArgs) -> Result<()> {
    let i18n = &ep.i18n;
    if !args.yes {
        confirm(&i18n.one("cli-user-confirm-disable", &args.username))?;
    }
    let path = format!("/admin/users/{}/disable", url_encode(&args.username));
    let _: UserResponse = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context(i18n.text("cli-user-disabling-user"))?;
    println!(
        "{}",
        i18n.format(
            "cli-user-disabled-user",
            &[("value0", args.username.to_string())]
        )
    );
    Ok(())
}

async fn enable(ep: &ServerEndpoint, args: UserEnableArgs) -> Result<()> {
    let i18n = &ep.i18n;
    let path = format!("/admin/users/{}/enable", url_encode(&args.username));
    let _: UserResponse = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context(i18n.text("cli-user-enabling-user"))?;
    println!(
        "{}",
        i18n.format(
            "cli-user-enabled-user",
            &[("value0", args.username.to_string())]
        )
    );
    Ok(())
}

#[derive(Debug, Serialize)]
struct PatchUserBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
}

async fn patch(ep: &ServerEndpoint, args: UserPatchArgs) -> Result<()> {
    let i18n = &ep.i18n;
    let body = PatchUserBody {
        name: args.name.map(|s| {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }),
        email: args.email.map(|s| {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }),
        role: args
            .role
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    };
    let path = format!("/admin/users/{}", url_encode(&args.username));
    let resp: UserResponse = patch_json(ep, &path, &body)
        .await
        .context(i18n.text("cli-user-patching-user"))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp.user)?);
    } else {
        println!(
            "{}",
            i18n.format(
                "cli-user-updated-user",
                &[
                    ("value0", display_username(&resp.user.username).to_string()),
                    ("value1", resp.user.status().to_string())
                ]
            )
        );
    }
    Ok(())
}

/// The one place a username reaches terminal output unchanged. CodeQL's
/// sensitive-data heuristic reads any `username` as account information; a
/// username is the operator-chosen login name, not a credential, and the
/// `rust/cleartext-logging` barrier model
/// (`.github/codeql/extensions/ai-memory-models/models/log-barriers.model.yml`)
/// names this function so printing one here raises no alert.
fn display_username(value: &str) -> &str {
    value
}

fn print_temp_password(
    i18n: &ai_memory_core::i18n::Localizer,
    resp: &UserWithPassword,
    json: bool,
    verb: &str,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "user": &resp.user,
                "temporary_password": &resp.temporary_password
            }))?
        );
    } else {
        let mut stderr = io::stderr().lock();
        let _ = writeln!(
            stderr,
            "{}",
            i18n.format(
                "cli-user-verb-user",
                &[
                    ("value0", verb.to_string()),
                    ("value1", resp.user.username.to_string())
                ]
            )
        );
        if let Some(name) = &resp.user.name {
            let _ = writeln!(
                stderr,
                "{}",
                i18n.format("cli-user-name-name", &[("value0", name.to_string())])
            );
        }
        if let Some(email) = &resp.user.email {
            let _ = writeln!(
                stderr,
                "{}",
                i18n.format("cli-user-email-email", &[("value0", email.to_string())])
            );
        }
        let _ = writeln!(
            stderr,
            "{}", i18n.format("cli-user-role-store-this-temporary-password-now-it-will-not-be-shown-again-the-user-m", &[("value0", resp.user.role.to_string())]));
        println!("{}", resp.temporary_password);
    }
    Ok(())
}

fn confirm(prompt: &str) -> Result<()> {
    let mut stderr = io::stderr().lock();
    let _ = write!(stderr, "{prompt}");
    let _ = stderr.flush();
    drop(stderr);

    let mut buf = String::new();
    let n = io::stdin().lock().read_line(&mut buf)?;
    if n == 0 {
        bail!("aborted (no input)");
    }
    let trimmed = buf.trim();
    if !trimmed.eq_ignore_ascii_case("y")
        && !trimmed.eq_ignore_ascii_case("yes")
        && !trimmed.eq_ignore_ascii_case("s")
        && !trimmed.eq_ignore_ascii_case("sim")
    {
        bail!("aborted");
    }
    Ok(())
}

pub(crate) fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encode_passes_safe_chars_through() {
        assert_eq!(url_encode("alice"), "alice");
        assert_eq!(url_encode("user_1"), "user_1");
        assert_eq!(url_encode("a.b-c"), "a.b-c");
    }

    #[test]
    fn url_encode_percent_encodes_at_and_other_specials() {
        assert_eq!(url_encode("alice@home"), "alice%40home");
        assert_eq!(url_encode("a/b"), "a%2Fb");
        assert_eq!(url_encode("a b"), "a%20b");
    }

    fn user(username: &str, name: Option<&str>) -> UserRow {
        UserRow {
            id: "u1".to_owned(),
            username: username.to_owned(),
            name: name.map(str::to_owned),
            email: None,
            created_at: 0,
            last_seen_at: None,
            role: "admin".to_owned(),
            must_change_password: false,
            disabled_at: None,
            has_password: true,
        }
    }

    /// The display-cell offset at which `line` carries `cell`, or `None`.
    fn cell_column(line: &str, cell: &str) -> Option<usize> {
        line.find(cell)
            .map(|byte| unicode_width::UnicodeWidthStr::width(&line[..byte]))
    }

    fn render(users: &[UserRow]) -> String {
        let mut buf = Vec::new();
        write_users(users, &mut buf).expect("rendering to a buffer cannot fail");
        String::from_utf8(buf).expect("the table is utf-8")
    }

    #[test]
    fn users_table_columns_line_up_past_ascii() {
        // Under a byte-measured width the double-width username is padded to
        // its byte count in characters, which pushes its NAME, ROLE and
        // STATUS cells right of the ASCII row's, and the accented name widens
        // NAME by one column no row needs.
        let rows = [user("利用者", Some("José")), user("bob", None)];
        let table = render(&rows);
        let header = table.lines().next().expect("a header row");
        // USERNAME ends in NAME, so look for the NAME header past its separator.
        let name_col = cell_column(header, "  NAME").expect("a NAME column") + 2;
        let role_col = cell_column(header, "ROLE").expect("a ROLE column");
        for line in table.lines().skip(1) {
            assert_eq!(
                cell_column(line, "admin"),
                Some(role_col),
                "ROLE column moved on {line:?}:\n{table}"
            );
        }
        let cjk_row = table.lines().nth(1).expect("the CJK row");
        assert_eq!(cell_column(cjk_row, "José"), Some(name_col), "{table}");
        let widest_name = "José".chars().count();
        assert_eq!(role_col, name_col + widest_name + 2, "{table}");

        // Control: ASCII rows line up under either measure, so the rows above
        // are what carry the assertions.
        let ascii = render(&[user("alice", Some("Jose")), user("bob", None)]);
        let ascii_header = ascii.lines().next().expect("a header row");
        let ascii_role = cell_column(ascii_header, "ROLE").expect("a ROLE column");
        for line in ascii.lines().skip(1) {
            assert_eq!(cell_column(line, "admin"), Some(ascii_role), "{ascii}");
        }
    }
}
