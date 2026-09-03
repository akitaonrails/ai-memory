//! YAML-frontmatter aware markdown parser and emitter.
//!
//! We deliberately do *not* use `gray_matter` here: it parses fine but
//! loses comments and key ordering on re-serialise, which is exactly the
//! "duplicate frontmatter on already-frontmatter'd files" class of bug
//! basic-memory hit (#528). Going through `serde_yaml` directly keeps the
//! round-trip predictable.

use std::collections::BTreeSet;

use ai_memory_core::{LinkTarget, PagePath};
use serde::{Deserialize, Serialize};

use crate::error::WikiResult;

/// A parsed markdown document with detached frontmatter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Markdown {
    /// Frontmatter as JSON for cheap querying (and stable serialisation).
    /// `Null` when the source had no frontmatter at all.
    pub frontmatter: serde_json::Value,
    /// Body excluding the frontmatter block (and the closing `---\n`).
    pub body: String,
}

/// Parse markdown text into [`Markdown`].
///
/// Recognises only the canonical `---\n<yaml>\n---\n` block at the very
/// start of the document. Anything else is treated as body.
///
/// # Errors
/// Returns [`WikiError::Yaml`] if the frontmatter block exists but does
/// not parse as YAML.
pub fn parse(input: &str) -> WikiResult<Markdown> {
    let trimmed = input.strip_prefix('\u{FEFF}').unwrap_or(input);
    if let Some(rest) = trimmed.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---\n")
    {
        let fm_str = &rest[..end];
        let body = rest[end + 5..].to_string();
        let fm_yaml: serde_yaml::Value = serde_yaml::from_str(fm_str)?;
        let fm_json: serde_json::Value = serde_json::to_value(fm_yaml)?;
        return Ok(Markdown {
            frontmatter: fm_json,
            body,
        });
    }
    Ok(Markdown {
        frontmatter: serde_json::Value::Null,
        body: input.to_string(),
    })
}

/// Emit a [`Markdown`] back to a string. Frontmatter is serialised through
/// `serde_yaml` (so it round-trips deterministically); a `Null` or empty
/// object frontmatter is omitted entirely.
///
/// # Errors
/// Returns [`WikiError::Yaml`] if frontmatter cannot be serialised.
pub fn emit(md: &Markdown) -> WikiResult<String> {
    let has_fm = match &md.frontmatter {
        serde_json::Value::Null => false,
        serde_json::Value::Object(m) => !m.is_empty(),
        _ => true,
    };
    let mut out = String::with_capacity(md.body.len() + 32);
    if has_fm {
        let yaml = serde_yaml::to_string(&md.frontmatter)?;
        out.push_str("---\n");
        out.push_str(&yaml);
        if !yaml.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("---\n");
    }
    out.push_str(&md.body);
    Ok(out)
}

/// Derive a page title.
///
/// Priority: frontmatter.title (string) → first `# ` heading in body →
/// path stem with the `.md` suffix stripped.
#[must_use]
pub fn derive_title(frontmatter: &serde_json::Value, body: &str, path: &PagePath) -> String {
    if let Some(t) = frontmatter.get("title").and_then(serde_json::Value::as_str) {
        let trimmed = t.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    let s = path.as_str();
    let stem = s.rsplit_once('/').map_or(s, |(_, name)| name);
    stem.strip_suffix(".md").unwrap_or(stem).to_string()
}

/// A normalised link key: `(workspace, project, path)`. `workspace` and
/// `project` are `None` for a link that resolves within the source page's
/// own project. Collected in a `BTreeSet` so output is deduped + stable.
type LinkKey = (Option<String>, Option<String>, String);

/// Extract internal wiki links from a markdown body.
///
/// Supports `[[wiki links]]`, `[[wiki links|labels]]`, cross-project
/// `[[project:path]]` / `[[workspace/project:path]]` wikilinks, and
/// ordinary markdown links such as `[label](../decisions/foo.md#anchor)`.
/// External URLs, anchors, images, and non-markdown assets are ignored.
/// Returned values are normalised to wiki-root-relative [`LinkTarget`]s.
#[must_use]
pub fn extract_links(body: &str, page_path: &PagePath) -> Vec<LinkTarget> {
    let mut out: BTreeSet<LinkKey> = BTreeSet::new();
    let mut in_fence = false;

    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        extract_wikilinks(line, page_path, &mut out);
        extract_markdown_links(line, page_path, &mut out);
    }

    out.into_iter()
        .filter_map(|(workspace, project, path)| {
            PagePath::new(path).ok().map(|path| LinkTarget {
                workspace,
                project,
                path,
                relation: None,
            })
        })
        .collect()
}

/// Body wikilinks plus typed `relations:` frontmatter edges — the full
/// outgoing link set every page-write path stores. One entry point so a
/// new writer cannot forget the typed half.
pub fn extract_all_links(
    frontmatter: &serde_json::Value,
    body: &str,
    page_path: &PagePath,
) -> Vec<LinkTarget> {
    let mut links = extract_links(body, page_path);
    links.extend(extract_relation_links(frontmatter));
    links
}

/// Upper bound (bytes) on an untrusted frontmatter value echoed into a
/// log line. Frontmatter is agent/operator-authored and, on a shared
/// server, one caller's page is parsed and logged by a process others
/// read the logs of; a relation key or target is meant to be a short
/// identifier, so bounding the logged form keeps a crafted or oversized
/// value from bloating or polluting the log without losing diagnostic
/// value. Mirrors the bounding every other untrusted-content sink uses.
const RELATION_LOG_FIELD_MAX_BYTES: usize = 200;

/// Bound an untrusted frontmatter value for safe logging.
fn log_bounded(value: &str) -> String {
    ai_memory_core::truncate_utf8_bytes(value, RELATION_LOG_FIELD_MAX_BYTES)
}

/// Extract typed relation edges from a page's `relations:` frontmatter
/// (2.0 item 3):
///
/// ```yaml
/// relations:
///   fixes: ["gotchas/build.md"]
///   contradicts: ["decisions/0007.md", "other-project:notes/x.md"]
/// ```
///
/// Values use the same target grammar as wikilinks (`path`,
/// `project:path`, `workspace/project:path`). Keys outside the closed
/// [`Relation`] vocabulary are skipped (a typo must not silently mint a
/// new edge kind); malformed paths are skipped likewise.
pub fn extract_relation_links(frontmatter: &serde_json::Value) -> Vec<LinkTarget> {
    let Some(relations) = frontmatter.get("relations").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, targets) in relations {
        let Some(relation) = ai_memory_core::Relation::parse(key) else {
            tracing::warn!(key = %log_bounded(key), "unknown relation key in frontmatter; skipping");
            continue;
        };
        let Some(list) = targets.as_array() else {
            continue;
        };
        for target in list.iter().filter_map(|v| v.as_str()) {
            let (workspace, project, raw_path) = match target.split_once(':') {
                None => (None, None, target),
                Some((scope, path)) => match scope.split_once('/') {
                    None => (None, Some(scope.to_string()), path),
                    Some((ws, proj)) => (Some(ws.to_string()), Some(proj.to_string()), path),
                },
            };
            // Same terminal normalization as wikilinks: extension-less
            // targets gain `.md`; anything with a non-md extension is
            // not a page and is skipped.
            let raw_path = raw_path.trim();
            let last = raw_path.rsplit_once('/').map_or(raw_path, |(_, s)| s);
            let normalized = if last.contains('.') {
                if raw_path.ends_with(".md") {
                    raw_path.to_string()
                } else {
                    tracing::warn!(target = %log_bounded(target), "relation target is not a page; skipping");
                    continue;
                }
            } else {
                format!("{raw_path}.md")
            };
            let Ok(path) = PagePath::new(normalized) else {
                tracing::warn!(target = %log_bounded(target), "unparseable relation target; skipping");
                continue;
            };
            out.push(LinkTarget {
                workspace,
                project,
                path,
                relation: Some(relation),
            });
        }
    }
    out
}

/// Split an optional `[workspace/]project:` scope qualifier off the front
/// of a wikilink target. Returns `(workspace, project, path_part)`. URL and
/// scheme-prefixed targets carry no scope (the `:` belongs to the scheme);
/// [`normalize_link_target`] rejects those downstream.
fn split_scope(target: &str) -> LinkKey {
    let lower = target.to_ascii_lowercase();
    if target.contains("://")
        || lower.starts_with("mailto:")
        || lower.starts_with("data:")
        || lower.starts_with("javascript:")
        || lower.starts_with("tel:")
    {
        return (None, None, target.to_string());
    }
    if let Some((scope, rest)) = target.split_once(':') {
        let scope = scope.trim();
        let scope_ok = !scope.is_empty()
            && scope
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'));
        if scope_ok {
            let (workspace, project) = match scope.split_once('/') {
                Some((ws, proj)) => (Some(ws.trim().to_string()), proj.trim()),
                None => (None, scope),
            };
            if !project.is_empty() {
                return (
                    workspace,
                    Some(project.to_string()),
                    rest.trim().to_string(),
                );
            }
        }
    }
    (None, None, target.to_string())
}

fn extract_wikilinks(line: &str, page_path: &PagePath, out: &mut BTreeSet<LinkKey>) {
    let mut rest = line;
    while let Some(start) = rest.find("[[") {
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find("]]") else {
            break;
        };
        let raw = &after_start[..end];
        // Strip the `|label` first, then peel any cross-project scope so the
        // remaining path normalises the same way a bare wikilink does.
        let unlabelled = raw.split_once('|').map_or(raw, |(target, _)| target).trim();
        let (workspace, project, path_part) = split_scope(unlabelled);
        if let Some(path) = normalize_link_target(&path_part, page_path, true) {
            out.insert((workspace, project, path));
        }
        rest = &after_start[end + 2..];
    }
}

fn extract_markdown_links(line: &str, page_path: &PagePath, out: &mut BTreeSet<LinkKey>) {
    let mut start_at = 0;
    while let Some(rel_start) = line[start_at..].find('[') {
        let start = start_at + rel_start;
        if start > 0 && line.as_bytes()[start - 1] == b'!' {
            start_at = start + 1;
            continue;
        }
        let after_start = start + 1;
        let Some(rel_close) = line[after_start..].find(']') else {
            break;
        };
        let close = after_start + rel_close;
        if !line[close + 1..].starts_with('(') {
            start_at = close + 1;
            continue;
        }
        let target_start = close + 2;
        let Some(rel_end) = line[target_start..].find(')') else {
            break;
        };
        let target_end = target_start + rel_end;
        let raw = &line[target_start..target_end];
        if let Some(path) = normalize_link_target(raw, page_path, false) {
            out.insert((None, None, path));
        }
        start_at = target_end + 1;
    }
}

fn normalize_link_target(raw: &str, page_path: &PagePath, wikilink: bool) -> Option<String> {
    let target = raw
        .split_once('|')
        .map_or(raw, |(path, _)| path)
        .trim()
        .trim_matches('<')
        .trim_matches('>');
    if target.is_empty() || target.starts_with('#') || target.contains("://") {
        return None;
    }
    let lower = target.to_ascii_lowercase();
    if lower.starts_with("mailto:")
        || lower.starts_with("data:")
        || lower.starts_with("javascript:")
        || lower.starts_with("tel:")
    {
        return None;
    }

    let target = target.split_once('#').map_or(target, |(path, _)| path);
    let target = target
        .split_once('?')
        .map_or(target, |(path, _)| path)
        .trim();
    if target.is_empty() || target.contains('\\') {
        return None;
    }

    let mut target = target.to_string();
    let last_segment = target.rsplit_once('/').map_or(target.as_str(), |(_, s)| s);
    if last_segment.contains('.') {
        if !target.ends_with(".md") {
            return None;
        }
    } else if wikilink || !last_segment.is_empty() {
        target.push_str(".md");
    }

    resolve_relative(page_path, &target, wikilink)
}

fn resolve_relative(page_path: &PagePath, target: &str, root_relative: bool) -> Option<String> {
    let mut parts: Vec<&str> = if root_relative || target.starts_with('/') {
        Vec::new()
    } else {
        page_path
            .as_str()
            .rsplit_once('/')
            .map_or_else(Vec::new, |(dir, _)| {
                dir.split('/').filter(|part| !part.is_empty()).collect()
            })
    };

    for part in target.trim_start_matches('/').split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> PagePath {
        PagePath::new("notes/here.md").unwrap()
    }

    #[test]
    fn untrusted_relation_values_are_bounded_before_logging() {
        // A crafted, oversized relation key/target must not reach the log
        // unbounded (security-audit: untrusted frontmatter -> log sink).
        let short = "fixes";
        assert_eq!(log_bounded(short), short, "short values pass through");

        let huge = "x".repeat(10_000);
        let bounded = log_bounded(&huge);
        assert!(
            bounded.len() <= RELATION_LOG_FIELD_MAX_BYTES,
            "logged value must be bounded: {} bytes",
            bounded.len()
        );

        // Never split a UTF-8 code point mid-truncation.
        let multibyte = "é".repeat(10_000);
        let bounded = log_bounded(&multibyte);
        assert!(bounded.len() <= RELATION_LOG_FIELD_MAX_BYTES);
        assert!(std::str::from_utf8(bounded.as_bytes()).is_ok());

        // A malicious relation block still yields no edges and does not
        // panic — the bound is applied on the skip path.
        let fm = serde_json::json!({
            "relations": { "x".repeat(5_000): ["ok.md"] }
        });
        assert!(extract_relation_links(&fm).is_empty());
    }

    #[test]
    fn relations_frontmatter_becomes_typed_edges() {
        let fm = serde_json::json!({
            "relations": {
                "fixes": ["gotchas/build.md", "concepts/writer"],
                "contradicts": ["other-proj:decisions/0007.md"],
                "causes": ["ws/proj:notes/x.md"],
            }
        });
        let mut links = extract_relation_links(&fm);
        links.sort();
        assert_eq!(links.len(), 4);
        let fixes: Vec<_> = links
            .iter()
            .filter(|l| l.relation == Some(ai_memory_core::Relation::Fixes))
            .collect();
        assert_eq!(fixes.len(), 2);
        // extension-less target gains .md
        assert!(
            fixes
                .iter()
                .any(|l| l.path.as_str() == "concepts/writer.md")
        );
        let contra = links
            .iter()
            .find(|l| l.relation == Some(ai_memory_core::Relation::Contradicts))
            .unwrap();
        assert_eq!(contra.project.as_deref(), Some("other-proj"));
        let causes = links
            .iter()
            .find(|l| l.relation == Some(ai_memory_core::Relation::Causes))
            .unwrap();
        assert_eq!(causes.workspace.as_deref(), Some("ws"));
        assert_eq!(causes.project.as_deref(), Some("proj"));
    }

    #[test]
    fn unknown_relation_keys_and_bad_targets_are_skipped() {
        let fm = serde_json::json!({
            "relations": {
                "blames": ["notes/a.md"],
                "fixes": ["../escape.md", "notes/data.json", "notes/ok.md"],
            }
        });
        let links = extract_relation_links(&fm);
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].path.as_str(), "notes/ok.md");
    }

    #[test]
    fn pages_without_relations_extract_nothing() {
        assert!(extract_relation_links(&serde_json::json!({})).is_empty());
        assert!(extract_relation_links(&serde_json::json!({"relations": "not a map"})).is_empty());
    }

    #[test]
    fn extract_all_links_merges_body_and_frontmatter() {
        let fm = serde_json::json!({"relations": {"fixes": ["gotchas/g.md"]}});
        let links = extract_all_links(&fm, "see [[notes/n.md]]", &page());
        assert_eq!(links.len(), 2);
        assert!(links.iter().any(|l| l.relation.is_none()));
        assert!(
            links
                .iter()
                .any(|l| l.relation == Some(ai_memory_core::Relation::Fixes))
        );
    }

    #[test]
    fn extract_links_bare_wikilink_is_local() {
        let links = extract_links("see [[decisions/0001.md]] and [[other]]", &page());
        assert!(links.iter().all(|l| !l.is_cross_project()));
        assert!(
            links
                .iter()
                .any(|l| l.path.as_str() == "decisions/0001.md" && l.project.is_none())
        );
        // bare name gets `.md` appended, still local
        assert!(links.iter().any(|l| l.path.as_str() == "other.md"));
    }

    #[test]
    fn extract_links_cross_project_wikilink() {
        let links = extract_links("dep on [[infra:runbooks/02.md]]", &page());
        let l = links.iter().find(|l| l.is_cross_project()).expect("xproj");
        assert_eq!(l.workspace, None);
        assert_eq!(l.project.as_deref(), Some("infra"));
        assert_eq!(l.path.as_str(), "runbooks/02.md");
    }

    #[test]
    fn extract_links_cross_workspace_wikilink_with_label() {
        let links = extract_links("[[zommehq/zomme:decisions/adr-1.md|the ADR]]", &page());
        let l = links.iter().find(|l| l.is_cross_project()).expect("xws");
        assert_eq!(l.workspace.as_deref(), Some("zommehq"));
        assert_eq!(l.project.as_deref(), Some("zomme"));
        assert_eq!(l.path.as_str(), "decisions/adr-1.md");
    }

    #[test]
    fn extract_links_url_wikilink_is_not_a_scope() {
        // `https://...` must not be parsed as project "https".
        let links = extract_links("[[https://example.com]] [[mailto:a@b.com]]", &page());
        assert!(links.is_empty(), "URLs/schemes are not links: {links:?}");
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let src = "---\ntitle: Hello\ntags:\n  - a\n  - b\n---\nThe body.\n";
        let md = parse(src).unwrap();
        assert_eq!(md.frontmatter["title"], "Hello");
        assert_eq!(md.frontmatter["tags"][0], "a");
        assert_eq!(md.body, "The body.\n");
    }

    #[test]
    fn parses_bom_prefixed_frontmatter() {
        let src = "\u{FEFF}---\ntitle: Hello\n---\nBody\n";
        let md = parse(src).unwrap();
        assert_eq!(md.frontmatter["title"], "Hello");
        assert_eq!(md.body, "Body\n");
    }

    #[test]
    fn malformed_frontmatter_returns_error() {
        let src = "---\ntitle: [unterminated\n---\nBody\n";
        assert!(parse(src).is_err());
    }

    #[test]
    fn unterminated_frontmatter_marker_is_body() {
        let src = "---\ntitle: Hello\nBody\n";
        let md = parse(src).unwrap();
        assert!(md.frontmatter.is_null());
        assert_eq!(md.body, src);
    }

    #[test]
    fn parses_body_without_frontmatter() {
        let src = "Just a body, no frontmatter.\n";
        let md = parse(src).unwrap();
        assert!(md.frontmatter.is_null());
        assert_eq!(md.body, src);
    }

    #[test]
    fn round_trip_emit_then_parse() {
        let original = Markdown {
            frontmatter: serde_json::json!({ "title": "X", "tags": ["a"] }),
            body: "Line 1\nLine 2\n".into(),
        };
        let emitted = emit(&original).unwrap();
        let parsed = parse(&emitted).unwrap();
        assert_eq!(parsed.frontmatter["title"], "X");
        assert_eq!(parsed.body, original.body);
    }

    #[test]
    fn round_trip_preserves_slot_kind_frontmatter() {
        let original = Markdown {
            frontmatter: serde_json::json!({
                "title": "Project context",
                "slot_kind": "invariant",
            }),
            body: "Stable project context.\n".into(),
        };
        let emitted = emit(&original).unwrap();
        let parsed = parse(&emitted).unwrap();
        assert_eq!(parsed.frontmatter["slot_kind"], "invariant");
        assert_eq!(parsed.body, original.body);
    }

    #[test]
    fn emit_omits_empty_frontmatter() {
        let md = Markdown {
            frontmatter: serde_json::Value::Object(serde_json::Map::new()),
            body: "Hello\n".into(),
        };
        assert_eq!(emit(&md).unwrap(), "Hello\n");
    }

    #[test]
    fn title_priority_frontmatter_then_heading_then_stem() {
        let path = PagePath::new("notes/foo.md").unwrap();
        // Frontmatter wins.
        let fm = serde_json::json!({ "title": "Explicit" });
        assert_eq!(derive_title(&fm, "# Other\nbody", &path), "Explicit");
        // Heading wins over stem.
        assert_eq!(
            derive_title(&serde_json::Value::Null, "# From Body\n", &path),
            "From Body"
        );
        // Stem fallback.
        assert_eq!(
            derive_title(&serde_json::Value::Null, "no heading", &path),
            "foo"
        );
    }

    #[test]
    fn extracts_internal_wiki_and_markdown_links() {
        let path = PagePath::new("concepts/current.md").unwrap();
        let body = "See [[decisions/0001-single-sqlite-file|SQLite]] and \
                    [gotcha](../gotchas/hooks.md#details). Also \
                    [external](https://example.com) and ![image](../img/logo.png).";
        let links = extract_links(body, &path);
        let paths: Vec<&str> = links.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["decisions/0001-single-sqlite-file.md", "gotchas/hooks.md"]
        );
    }

    #[test]
    fn extract_links_ignores_fenced_code_blocks() {
        let path = PagePath::new("notes/a.md").unwrap();
        let body = "```\n[[notes/ignored]]\n```\n[[notes/kept]]\n";
        let links = extract_links(body, &path);
        let paths: Vec<&str> = links.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(paths, vec!["notes/kept.md"]);
    }
}
