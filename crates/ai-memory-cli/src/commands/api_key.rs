//! `ai-memory api-key` — manage native `aim_` API credentials.
//!
//! Thin HTTP client over `/admin/api-credentials*`. Root bearer required.

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::{
    ApiKeyAddArgs, ApiKeyArgs, ApiKeyCommand, ApiKeyListArgs, ApiKeyRevokeArgs, ApiKeyRotateArgs,
};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json, post_json};

#[derive(Debug, Deserialize, Serialize)]
struct CredentialRow {
    id: String,
    user_id: String,
    label: String,
    #[serde(default)]
    preview: Option<String>,
    created_at: i64,
    #[serde(default)]
    last_used_at: Option<i64>,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    revoked_at: Option<i64>,
}

impl CredentialRow {
    fn status(&self) -> &'static str {
        if self.revoked_at.is_some() {
            "revoked"
        } else {
            "active"
        }
    }
}

#[derive(Debug, Deserialize)]
struct CredentialWithToken {
    credential: CredentialRow,
    token: String,
}

#[derive(Debug, Deserialize)]
struct CredentialList {
    credentials: Vec<CredentialRow>,
}

/// Dispatch `ai-memory api-key`.
///
/// # Errors
/// HTTP failure or malformed JSON.
pub async fn run(config: &Config, args: ApiKeyArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    match args.command {
        ApiKeyCommand::Add(args) => add(&ep, args).await,
        ApiKeyCommand::List(args) => list(&ep, args).await,
        ApiKeyCommand::Rotate(args) => rotate(&ep, args).await,
        ApiKeyCommand::Revoke(args) => revoke(&ep, args).await,
    }
}

#[derive(Debug, Serialize)]
struct CreateBody<'a> {
    username: &'a str,
    label: &'a str,
}

async fn add(ep: &ServerEndpoint, args: ApiKeyAddArgs) -> Result<()> {
    let i18n = &ep.i18n;
    let body = CreateBody {
        username: args.username.trim(),
        label: args.label.trim(),
    };
    let resp: CredentialWithToken = post_json(ep, "/admin/api-credentials", &body)
        .await
        .context(i18n.text("cli-api-key-creating-api-credential"))?;
    print_token(i18n, &resp, args.json, "created")
}

async fn list(ep: &ServerEndpoint, args: ApiKeyListArgs) -> Result<()> {
    let i18n = &ep.i18n;
    let resp: CredentialList = get_json(ep, "/admin/api-credentials", &[])
        .await
        .context(i18n.text("cli-api-key-listing-api-credentials"))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp.credentials)?);
        return Ok(());
    }
    if resp.credentials.is_empty() {
        println!("{}", i18n.text("cli-api-key-no-api-credentials"));
        return Ok(());
    }
    write_credentials(i18n, &resp.credentials, &mut io::stdout().lock())
}

/// Lay out the credentials table using terminal display-cell widths. Labels
/// are free text, so a byte count over-pads an accented label and misplaces
/// every cell after a double-width one.
fn write_credentials(
    i18n: &ai_memory_core::i18n::Localizer,
    credentials: &[CredentialRow],
    out: &mut impl Write,
) -> Result<()> {
    let id_w = credentials
        .iter()
        .map(|c| unicode_width::UnicodeWidthStr::width(c.id.as_str()))
        .max()
        .unwrap_or(8)
        .max(8);
    let label_w = credentials
        .iter()
        .map(|c| unicode_width::UnicodeWidthStr::width(c.label.as_str()))
        .max()
        .unwrap_or(5)
        .max(5);
    writeln!(
        out,
        "{}",
        i18n.diagnostic(&format!(
            "{}  {}  {:<8}  PREVIEW",
            pad_cell("ID", id_w),
            pad_cell("LABEL", label_w),
            "STATUS"
        ))
    )?;
    for c in credentials {
        writeln!(
            out,
            "{}  {}  {:<8}  {}",
            pad_cell(&c.id, id_w),
            pad_cell(&c.label, label_w),
            c.status(),
            c.preview.as_deref().unwrap_or("-")
        )?;
    }
    Ok(())
}

fn pad_cell(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(unicode_width::UnicodeWidthStr::width(value));
    format!("{value}{}", " ".repeat(padding))
}

async fn rotate(ep: &ServerEndpoint, args: ApiKeyRotateArgs) -> Result<()> {
    let i18n = &ep.i18n;
    if !args.yes {
        confirm(&i18n.one("cli-api-key-confirm-rotate", &args.id))?;
    }
    let path = format!("/admin/api-credentials/{}/rotate", args.id.trim());
    let resp: CredentialWithToken = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context(i18n.text("cli-api-key-rotating-api-credential"))?;
    print_token(i18n, &resp, args.json, "rotated")
}

async fn revoke(ep: &ServerEndpoint, args: ApiKeyRevokeArgs) -> Result<()> {
    let i18n = &ep.i18n;
    if !args.yes {
        confirm(&i18n.one("cli-api-key-confirm-revoke", &args.id))?;
    }
    let path = format!("/admin/api-credentials/{}/revoke", args.id.trim());
    let _: serde_json::Value = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context(i18n.text("cli-api-key-revoking-api-credential"))?;
    println!(
        "{}",
        i18n.format(
            "cli-api-key-revoked-api-key",
            &[("value0", args.id.to_string())]
        )
    );
    Ok(())
}

fn print_token(
    i18n: &ai_memory_core::i18n::Localizer,
    resp: &CredentialWithToken,
    json: bool,
    verb: &str,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "credential": &resp.credential,
                "token": &resp.token
            }))?
        );
    } else {
        let mut stderr = io::stderr().lock();
        let _ = writeln!(
            stderr,
            "{}",
            i18n.format(
                "cli-api-key-verb-api-key-store-this-token-now-it-will-not-be-shown-again",
                &[
                    ("value0", verb.to_string()),
                    ("value1", resp.credential.id.to_string()),
                    ("value2", resp.credential.label.to_string())
                ]
            )
        );
        println!("{}", resp.token);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(label: &str) -> CredentialRow {
        CredentialRow {
            id: "k1".to_owned(),
            user_id: "u1".to_owned(),
            label: label.to_owned(),
            preview: Some("aim_ab".to_owned()),
            created_at: 0,
            last_used_at: None,
            expires_at: None,
            revoked_at: None,
        }
    }

    /// The display-cell offset at which `line` carries `cell`, or `None`.
    fn cell_column(line: &str, cell: &str) -> Option<usize> {
        line.find(cell)
            .map(|byte| unicode_width::UnicodeWidthStr::width(&line[..byte]))
    }

    fn render(credentials: &[CredentialRow]) -> String {
        let mut buf = Vec::new();
        write_credentials(
            &ai_memory_core::i18n::Localizer::default(),
            credentials,
            &mut buf,
        )
        .expect("rendering to a buffer cannot fail");
        String::from_utf8(buf).expect("the table is utf-8")
    }

    #[test]
    fn credentials_table_columns_line_up_past_ascii() {
        // Under a byte-measured width the double-width label is padded to its
        // byte count in characters, which pushes its STATUS and PREVIEW cells
        // right of the ASCII row's.
        let table = render(&[credential("記憶"), credential("ci")]);
        let header = table.lines().next().expect("a header row");
        let status_col = cell_column(header, "STATUS").expect("a STATUS column");
        for line in table.lines().skip(1) {
            assert_eq!(
                cell_column(line, "active"),
                Some(status_col),
                "STATUS column moved on {line:?}:\n{table}"
            );
        }

        // An accented label must not widen LABEL past what any row needs:
        // STATUS follows LABEL by the widest label plus the separator.
        let accented = render(&[credential("produção"), credential("ci")]);
        let header = accented.lines().next().expect("a header row");
        let label_col = cell_column(header, "LABEL").expect("a LABEL column");
        let status_col = cell_column(header, "STATUS").expect("a STATUS column");
        let widest_label = "produção".chars().count();
        assert_eq!(status_col, label_col + widest_label + 2, "{accented}");

        // Control: ASCII rows line up under either measure, so the rows above
        // are what carry the assertions.
        let ascii = render(&[credential("prod"), credential("ci")]);
        let ascii_header = ascii.lines().next().expect("a header row");
        let ascii_status = cell_column(ascii_header, "STATUS").expect("a STATUS column");
        for line in ascii.lines().skip(1) {
            assert_eq!(cell_column(line, "active"), Some(ascii_status), "{ascii}");
        }
    }
}
