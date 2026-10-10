//! The on-disk page format: a small, fixed frontmatter plus the verbatim
//! server body.
//!
//! `memory_write_page` replaces a whole page and clears every field the call
//! omits, so a file must carry the metadata an import sends back. Only the
//! fields that write accepts and this tool round-trips appear: `title`,
//! `tags`, `pinned`, and `tier` when it is not the server default. Values
//! are JSON scalars and arrays, which are also valid YAML, so a reviewer
//! reads plain YAML and the parser stays strict and dependency-free.

use anyhow::{Result, bail};
use serde_json::Value;

/// The tier `memory_write_page` assigns when the call omits one.
pub const DEFAULT_TIER: &str = "semantic";
const FENCE: &str = "---";

/// Page metadata a file carries and an import sends back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageMeta {
    pub title: Option<String>,
    pub tags: Vec<String>,
    pub pinned: bool,
    /// `None` means the server default ([`DEFAULT_TIER`]).
    pub tier: Option<String>,
}

impl PageMeta {
    /// Metadata of a server page, normalised so a default tier and an empty
    /// title are omitted rather than written.
    pub fn from_server(title: &str, tier: &str, pinned: bool, frontmatter: &Value) -> Self {
        let tags = frontmatter
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            title: (!title.is_empty()).then(|| title.to_owned()),
            tags,
            pinned,
            tier: (!tier.is_empty() && tier != DEFAULT_TIER).then(|| tier.to_owned()),
        }
    }
}

/// Render the file for a page: frontmatter, then the body byte for byte.
pub fn render(meta: &PageMeta, body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 128);
    out.push_str(FENCE);
    out.push('\n');
    if let Some(title) = &meta.title {
        out.push_str("title: ");
        out.push_str(&json_string(title));
        out.push('\n');
    }
    if !meta.tags.is_empty() {
        let tags: Vec<String> = meta.tags.iter().map(|tag| json_string(tag)).collect();
        out.push_str("tags: [");
        out.push_str(&tags.join(", "));
        out.push_str("]\n");
    }
    if meta.pinned {
        out.push_str("pinned: true\n");
    }
    if let Some(tier) = &meta.tier {
        out.push_str("tier: ");
        out.push_str(&json_string(tier));
        out.push('\n');
    }
    out.push_str(FENCE);
    out.push('\n');
    out.push_str(body);
    out
}

fn json_string(value: &str) -> String {
    Value::String(value.to_owned()).to_string()
}

/// Parse a file this tool wrote, or a reviewer edited, back into metadata
/// and body. Strict on purpose: an import replaces the whole server page,
/// so a key this tool does not understand is refused rather than dropped.
/// Plain (unquoted) scalars are accepted for `title` and `tier`, the shape a
/// person editing YAML by hand most often writes.
pub fn parse(bytes: &[u8]) -> Result<(PageMeta, String)> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        bail!("file is not valid UTF-8");
    };
    let mut lines = text.split_inclusive('\n');
    let opening = lines.next().unwrap_or_default();
    if trim_eol(opening) != FENCE {
        bail!(
            "file has no frontmatter; an import needs it, because the server clears \
             any title, tags or pin the write omits"
        );
    }
    let mut consumed = opening.len();
    let mut meta = PageMeta::default();
    let mut seen: Vec<&str> = Vec::new();
    let mut closed = false;
    for line in lines {
        consumed += line.len();
        let line = trim_eol(line);
        if line == FENCE {
            closed = true;
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            bail!("frontmatter line {line:?} is not `key: value`");
        };
        let key = key.trim();
        let value = value.trim();
        if seen.contains(&key) {
            bail!("frontmatter key {key:?} appears twice");
        }
        match key {
            "title" => meta.title = Some(scalar(key, value)?),
            "tier" => meta.tier = Some(scalar(key, value)?).filter(|tier| tier != DEFAULT_TIER),
            "pinned" => {
                meta.pinned = match value {
                    "true" => true,
                    "false" => false,
                    _ => bail!("frontmatter `pinned` must be true or false, not {value:?}"),
                }
            }
            "tags" => meta.tags = tags(value)?,
            _ => bail!(
                "frontmatter key {key:?} is not one wikisync round-trips \
                 (title, tags, pinned, tier); remove it before importing"
            ),
        }
        seen.push(key);
    }
    if !closed {
        bail!("frontmatter is not closed by a `---` line");
    }
    Ok((meta, text[consumed..].to_owned()))
}

fn trim_eol(line: &str) -> &str {
    line.strip_suffix('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .unwrap_or(line)
}

fn scalar(key: &str, value: &str) -> Result<String> {
    let parsed = if value.starts_with('"') {
        match serde_json::from_str::<Value>(value) {
            Ok(Value::String(text)) => text,
            _ => bail!("frontmatter `{key}` is not a valid double-quoted string: {value}"),
        }
    } else if value.starts_with(['\'', '[', '{', '|', '>', '&', '*', '!']) {
        bail!("frontmatter `{key}` must be plain text or a double-quoted string");
    } else {
        value.to_owned()
    };
    if parsed.trim().is_empty() || parsed.contains(['\n', '\r']) {
        bail!("frontmatter `{key}` must be one non-empty line");
    }
    Ok(parsed)
}

fn tags(value: &str) -> Result<Vec<String>> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(value) else {
        bail!("frontmatter `tags` must be a list of double-quoted strings, like [\"db\", \"ops\"]");
    };
    items
        .into_iter()
        .map(|item| match item {
            Value::String(tag) if !tag.trim().is_empty() => Ok(tag),
            other => bail!("frontmatter tag {other} is not a non-empty string"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta() -> PageMeta {
        PageMeta {
            title: Some("Use \"Postgres\": ação".to_string()),
            tags: vec!["db".to_string(), "ops".to_string()],
            pinned: true,
            tier: Some("episodic".to_string()),
        }
    }

    #[test]
    fn render_then_parse_round_trips_metadata_and_body() {
        let body = "# Use Postgres\n\n---\nA rule line inside the body.\n";
        let rendered = render(&meta(), body);
        assert!(rendered.starts_with("---\ntitle: "), "{rendered}");
        let (parsed, parsed_body) = parse(rendered.as_bytes()).unwrap();
        assert_eq!(parsed, meta());
        assert_eq!(parsed_body, body);
    }

    #[test]
    fn defaults_are_omitted_from_the_file() {
        let plain = PageMeta {
            title: Some("T".to_string()),
            ..PageMeta::default()
        };
        assert_eq!(render(&plain, "# T\n"), "---\ntitle: \"T\"\n---\n# T\n");
        let server = PageMeta::from_server("T", DEFAULT_TIER, false, &json!({"tier": "semantic"}));
        assert_eq!(server, plain);
        let tagged = PageMeta::from_server("T", "episodic", true, &json!({"tags": ["a", 1, "b"]}));
        assert_eq!(tagged.tags, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(tagged.tier.as_deref(), Some("episodic"));
    }

    #[test]
    fn hand_edited_yaml_is_accepted_when_unambiguous() {
        let file = "---\r\ntitle: Plain title\r\n\r\ntier: semantic\r\npinned: false\r\ntags: []\r\n---\r\nbody\r\n";
        let (meta, body) = parse(file.as_bytes()).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Plain title"));
        assert_eq!(meta.tier, None, "the default tier normalises away");
        assert!(meta.tags.is_empty() && !meta.pinned);
        assert_eq!(body, "body\r\n");
    }

    #[test]
    fn anything_an_import_could_lose_is_refused() {
        for (file, needle) in [
            ("# no frontmatter\n", "no frontmatter"),
            (
                "---\ntitle: \"T\"\nbody without a closing fence\n",
                "not `key: value`",
            ),
            ("---\ntitle: \"T\"\n", "not closed"),
            ("---\nsummary: \"x\"\n---\n", "not one wikisync round-trips"),
            ("---\ntitle: \"a\"\ntitle: \"b\"\n---\n", "twice"),
            ("---\npinned: yes\n---\n", "true or false"),
            ("---\ntags: db, ops\n---\n", "list of double-quoted"),
            ("---\ntags: [\"\"]\n---\n", "non-empty string"),
            (
                "---\ntitle: 'single'\n---\n",
                "plain text or a double-quoted",
            ),
            ("---\ntitle: \"\"\n---\n", "non-empty line"),
            ("---\ntitle: \"a\\nb\"\n---\n", "non-empty line"),
        ] {
            let err = parse(file.as_bytes()).unwrap_err().to_string();
            assert!(err.contains(needle), "{file:?}: {err}");
        }
        assert!(
            parse(&[0xff, 0xfe])
                .unwrap_err()
                .to_string()
                .contains("UTF-8")
        );
    }
}
