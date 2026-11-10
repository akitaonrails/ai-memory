//! YAML-frontmatter aware markdown parser and emitter.
//!
//! We deliberately do *not* use `gray_matter` here: it parses fine but
//! loses comments and key ordering on re-serialise, which is exactly the
//! "duplicate frontmatter on already-frontmatter'd files" class of bug
//! basic-memory hit (#528). Reading through [`crate::frontmatter_yaml`] and
//! writing through `serde-saphyr` directly keeps the round-trip predictable.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use ai_memory_core::{DEFAULT_WORKSPACE_NAME, GLOBAL_SCOPE_PROJECT, LinkTarget, PagePath};
use serde::{Deserialize, Serialize};

use crate::error::{WikiError, WikiResult};

/// `yaml_12` emits plain scalars the way `serde_yaml` 0.9 did (no quotes
/// around dates or `yes`), but prefixes this header, which a frontmatter
/// block never carried.
const YAML_12_HEADER: &str = "%YAML 1.2\n---\n";

/// Serializer options for frontmatter, chosen to match `serde_yaml` 0.9
/// output. The one remaining difference: a value that needs quoting gets
/// double quotes where `serde_yaml` used single quotes.
fn frontmatter_emit_options() -> serde_saphyr::SerializerOptions {
    serde_saphyr::ser_options! {
        yaml_12: true,
        // Never fold long single-line strings into `>` blocks.
        folded_wrap_chars: usize::MAX,
    }
}

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
/// start of the document, with either LF or CRLF line endings on the fence
/// lines. The closing fence may also end the file without a line ending.
/// Anything else is treated as body.
///
/// A leading UTF-8 BOM is dropped either way. It only means "this file is
/// UTF-8" while it sits at offset zero; carried into `body` it is a
/// zero-width no-break space in front of the first line, which hides an H1
/// from [`derive_title`] and rides into the body a later re-emit writes
/// back after the frontmatter fence.
///
/// CRLF is what a Windows editor saves, and what `core.autocrlf=true` checks
/// out for every page of a wiki cloned onto Windows. Missing the fence there
/// treats the block as body: the page reindexes with no tier, pin or TTL, and
/// the in-place OKF file pass writes a second frontmatter block above it. The
/// body keeps its line endings untouched either way.
///
/// # Errors
/// Returns [`WikiError::Yaml`] if the frontmatter block exists but does
/// not parse as YAML.
pub fn parse(input: &str) -> WikiResult<Markdown> {
    let trimmed = input.strip_prefix('\u{FEFF}').unwrap_or(input);
    let (rest, newline) = if let Some(rest) = trimmed.strip_prefix("---\r\n") {
        (Some(rest), "\r\n")
    } else {
        (trimmed.strip_prefix("---\n"), "\n")
    };
    let close = format!("\n---{newline}");
    // The closing fence may also be the file's last line with no line
    // ending after it: a frontmatter-only file (a `_meta.md` scope manifest,
    // a stub page) saved by an editor that does not add a final newline.
    let split = rest.and_then(|rest| {
        // An empty block puts the closing fence first, with no line before
        // it to supply the newline `close` starts with.
        if let Some(body) = rest.strip_prefix(&close[1..]) {
            return Some(("", body));
        }
        if rest == "---" {
            return Some(("", ""));
        }
        rest.find(&close)
            .map(|end| (&rest[..end], &rest[end + close.len()..]))
            .or_else(|| rest.strip_suffix("\n---").map(|fm| (fm, "")))
    });
    if let Some((fm_str, body)) = split {
        let fm_str = fm_str.trim_end_matches('\r');
        let body = body.to_string();
        let fm_json = crate::frontmatter_yaml::parse(fm_str).map_err(WikiError::Yaml)?;
        return Ok(Markdown {
            frontmatter: fm_json,
            body,
        });
    }
    Ok(Markdown {
        frontmatter: serde_json::Value::Null,
        body: trimmed.to_string(),
    })
}

/// Emit a [`Markdown`] back to a string. Frontmatter is serialised through
/// `serde-saphyr` (so it round-trips deterministically); a `Null` or empty
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
        let yaml =
            serde_saphyr::to_string_with_options(&md.frontmatter, frontmatter_emit_options())?;
        let yaml = yaml.strip_prefix(YAML_12_HEADER).unwrap_or(&yaml);
        out.push_str("---\n");
        out.push_str(yaml);
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
/// Priority: frontmatter.title (string) → first `# ` heading in body
/// outside fenced code blocks → path stem with the `.md` suffix stripped.
#[must_use]
pub fn derive_title(frontmatter: &serde_json::Value, body: &str, path: &PagePath) -> String {
    if let Some(t) = frontmatter.get("title").and_then(serde_json::Value::as_str) {
        let trimmed = t.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let mut fence = None;
    for line in body.lines() {
        // A `# ` line inside a fenced block is a shell or Python comment,
        // not a heading.
        let (next, in_code) = CodeFence::step(fence, line);
        fence = next;
        if in_code {
            continue;
        }
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

/// Active code fence delimiter and its opening run length (CommonMark §4.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CodeFence {
    glyph: char,
    len: usize,
}

impl CodeFence {
    /// Update the fence state based on `line`.
    ///
    /// Returns `(updated_fence_state, line_is_code_or_fence)`.
    fn step(current: Option<Self>, line: &str) -> (Option<Self>, bool) {
        let in_fence = current.is_some();
        let leading_spaces = line.as_bytes().iter().take_while(|&&b| b == b' ').count();
        if leading_spaces > 3 {
            return (current, in_fence);
        }
        let trimmed = &line[leading_spaces..];
        let glyph = match trimmed.chars().next() {
            Some(c @ ('`' | '~')) => c,
            _ => return (current, in_fence),
        };
        let count = trimmed.chars().take_while(|&c| c == glyph).count();
        if count < 3 {
            return (current, in_fence);
        }
        let info = &trimmed[count..];
        match current {
            // A backtick fence's info string cannot contain a backtick, so
            // ```` ```code``` text ```` is a paragraph with an inline span,
            // not a fence that would swallow the rest of the page.
            None if glyph == '`' && info.contains('`') => (None, false),
            None => (Some(CodeFence { glyph, len: count }), true),
            Some(fence) if fence.glyph == glyph && count >= fence.len && info.trim().is_empty() => {
                (None, true)
            }
            Some(fence) => (Some(fence), true),
        }
    }
}

/// Extract internal wiki links from a markdown body.
///
/// Supports `[[wiki links]]`, `[[wiki links|labels]]`, cross-project
/// `[[project:path]]` / `[[workspace/project:path]]` wikilinks, the reserved
/// `[[_global:path]]` form (always the `_global` project in the default
/// workspace), and ordinary markdown links such as
/// `[label](../decisions/foo.md#anchor)`.
/// External URLs, anchors, images, and non-markdown assets are ignored, and
/// so is anything inside a fenced block or an inline code span, which the
/// page shows as code rather than as a link.
/// Returned values are normalised to wiki-root-relative [`LinkTarget`]s.
#[must_use]
pub fn extract_links(body: &str, page_path: &PagePath) -> Vec<LinkTarget> {
    let mut out: BTreeSet<LinkKey> = BTreeSet::new();
    let mut fence: Option<CodeFence> = None;

    for line in body.lines() {
        let (next_fence, is_fence_or_code) = CodeFence::step(fence, line);
        fence = next_fence;
        if is_fence_or_code {
            continue;
        }
        let line = blank_inline_code(line);
        extract_wikilinks(&line, page_path, &mut out);
        extract_markdown_links(&line, page_path, &mut out);
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

/// `line` with each inline code span, backticks included, replaced by
/// spaces. The web renderer shows `` `[[notes/x]]` `` as code, so indexing
/// it minted a backlink the source page never offers and, for a
/// `[[project:path]]` example, a broken-link lint finding. Blanking rather
/// than cutting keeps a link whose label is code (`` [`foo`](foo.md) ``)
/// intact. A run of backticks opens a span only a run of the same length
/// closes (CommonMark 6.1); with no closer on the line it is literal text.
/// A span that continues onto the next line is not seen, as the line-based
/// fence check above does not see an indented code block either.
fn blank_inline_code(line: &str) -> std::borrow::Cow<'_, str> {
    let spans = inline_code_spans(line);
    if spans.is_empty() {
        return std::borrow::Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    for span in spans {
        out.push_str(&line[copied..span.start]);
        out.extend(std::iter::repeat_n(' ', line[span.clone()].chars().count()));
        copied = span.end;
    }
    out.push_str(&line[copied..]);
    std::borrow::Cow::Owned(out)
}

/// Byte ranges of inline code spans in `line`, in document order and
/// without overlap. A run of `open` backticks opens a span; only a run of
/// the same length closes it (CommonMark 6.1). A run with no matching
/// closer on the line is literal text, not a span. Shared by
/// [`blank_inline_code`] (link extraction) and the wikilink-to-Markdown
/// rewriter so both treat "what the renderer shows as code" identically.
fn inline_code_spans(line: &str) -> Vec<Range<usize>> {
    if !line.contains('`') {
        return Vec::new();
    }
    let bytes = line.as_bytes();
    let run_at = |i: usize| bytes[i..].iter().take_while(|&&b| b == b'`').count();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let open = run_at(i);
        let mut j = i + open;
        let close = loop {
            if j >= bytes.len() {
                break None;
            }
            if bytes[j] == b'`' {
                let run = run_at(j);
                if run == open {
                    break Some(j + run);
                }
                j += run;
            } else {
                j += 1;
            }
        };
        match close {
            Some(end) => {
                spans.push(i..end);
                i = end;
            }
            None => i += open,
        }
    }
    spans
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

/// Whether a link target's final path segment can name a page.
///
/// A directory target (trailing `/`, so an empty last segment) and a
/// stem-less `.md` cannot: the first used to normalize to the literal
/// `notes/.md` in `relations:` frontmatter, the second to an
/// extension-less path — both permanently unresolved `links` rows that no
/// page write could ever repoint.
fn last_segment_names_a_page(target: &str) -> bool {
    let last = target.rsplit_once('/').map_or(target, |(_, s)| s);
    !last.is_empty() && last != ".md"
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
            let (workspace, project, raw_path) = split_scope(target);
            // Same terminal normalization as wikilinks: extension-less
            // targets gain `.md`; a directory target, a stem-less `.md`, and
            // anything with a non-md extension are not pages and are skipped.
            let raw_path = raw_path.trim();
            let last = raw_path.rsplit_once('/').map_or(raw_path, |(_, s)| s);
            let names_a_page = last_segment_names_a_page(raw_path)
                && (!last.contains('.') || raw_path.ends_with(".md"));
            if !names_a_page {
                tracing::warn!(target = %log_bounded(target), "relation target is not a page; skipping");
                continue;
            }
            let normalized = if last.contains('.') {
                raw_path.to_string()
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
///
/// `[[_global:path]]` is a project-only qualifier, but the reserved
/// preferences project lives in [`DEFAULT_WORKSPACE_NAME`], not in the
/// source page's workspace (issue #1042). An explicit
/// `[[workspace/_global:path]]` keeps the named workspace.
fn split_scope(target: &str) -> LinkKey {
    let lower = target.to_ascii_lowercase();
    if target.contains("://")
        || lower.starts_with("mailto:")
        || lower.starts_with("data:")
        || lower.starts_with("javascript:")
        || lower.starts_with("vbscript:")
        || lower.starts_with("tel:")
        || lower.starts_with("file:")
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
            let (mut workspace, project) = match scope.split_once('/') {
                Some((ws, proj)) => (Some(ws.trim().to_string()), proj.trim()),
                None => (None, scope),
            };
            if !project.is_empty() {
                if workspace.is_none() && project == GLOBAL_SCOPE_PROJECT {
                    workspace = Some(DEFAULT_WORKSPACE_NAME.to_string());
                }
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

/// Rewrite `body`'s LOCAL wikilinks (`[[target]]`, `[[target|label]]`, no
/// scope prefix, or an explicit scope prefix naming `own_project`) into
/// bundle-relative standard Markdown links (`[label](relative/path.md)`),
/// computed from `page_path`'s own directory depth. A cross-project
/// (`[[project:path]]`) or cross-workspace (`[[workspace/project:path]]`)
/// wikilink that does not resolve to `own_project` is left as literal
/// `[[...]]` text — no bundle-relative equivalent exists across bundle
/// boundaries. Export-only: this never runs on the write path, so it never
/// touches an on-disk wiki file.
///
/// Skips fenced code blocks and inline code spans exactly like
/// [`extract_links`] does, reusing the same fence/inline-code detection so
/// the two never disagree about what counts as code. Malformed or empty
/// targets are left untouched, mirroring [`extract_links`]'s own leniency;
/// this never panics on any body content.
#[must_use]
pub fn rewrite_local_wikilinks(body: &str, page_path: &PagePath, own_project: &str) -> String {
    let mut out = String::with_capacity(body.len() + 64);
    let mut fence: Option<CodeFence> = None;
    for raw_line in body.split_inclusive('\n') {
        let (line, terminator) = split_line_terminator(raw_line);
        let (next_fence, is_fence_or_code) = CodeFence::step(fence, line);
        fence = next_fence;
        if is_fence_or_code {
            out.push_str(line);
            out.push_str(terminator);
            continue;
        }
        rewrite_wikilinks_in_line(line, page_path, own_project, &mut out);
        out.push_str(terminator);
    }
    out
}

/// Split a `split_inclusive('\n')` line into its content and line-ending,
/// so a rewrite can copy the ending back verbatim (LF or CRLF).
fn split_line_terminator(raw: &str) -> (&str, &str) {
    if let Some(stripped) = raw.strip_suffix("\r\n") {
        (stripped, "\r\n")
    } else if let Some(stripped) = raw.strip_suffix('\n') {
        (stripped, "\n")
    } else {
        (raw, "")
    }
}

fn rewrite_wikilinks_in_line(
    line: &str,
    page_path: &PagePath,
    own_project: &str,
    out: &mut String,
) {
    let code_spans = inline_code_spans(line);
    let mut pos = 0;
    for span in &code_spans {
        rewrite_wikilinks_in_segment(&line[pos..span.start], page_path, own_project, out);
        out.push_str(&line[span.clone()]);
        pos = span.end;
    }
    rewrite_wikilinks_in_segment(&line[pos..], page_path, own_project, out);
}

fn rewrite_wikilinks_in_segment(
    seg: &str,
    page_path: &PagePath,
    own_project: &str,
    out: &mut String,
) {
    let mut rest = seg;
    loop {
        let Some(start) = rest.find("[[") else {
            out.push_str(rest);
            return;
        };
        out.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find("]]") else {
            out.push_str(&rest[start..]);
            return;
        };
        let raw = &after_start[..end];
        let resolved = resolve_local_wikilink(raw, page_path, own_project)
            .and_then(|(href, label)| Some((markdown_destination(&href)?, label)));
        match resolved {
            Some((destination, label)) => {
                out.push('[');
                out.push_str(&escape_markdown_link_label(&label));
                out.push_str("](");
                out.push_str(&destination);
                out.push(')');
            }
            None => {
                out.push_str("[[");
                out.push_str(raw);
                out.push_str("]]");
            }
        }
        rest = &after_start[end + 2..];
    }
}

/// Rewrite `body`, replacing each wikilink whose normalised
/// `(workspace, project, path)` key appears in `delink` with its plain label
/// text: the `|label` when one is present, otherwise the raw target text.
/// Every other wikilink — resolved cross-scope links and all same-project
/// links included — is copied byte-identically, and fenced code blocks /
/// inline code spans are skipped exactly as [`extract_links`] skips them, so
/// an example `[[project:path]]` shown as code is never rewritten.
///
/// The consolidation apply path uses this to strip cross-scope links the
/// store cannot resolve (#1218): the `delink` set is built from
/// [`extract_links`] keys, so a key computed here matches byte-for-byte.
/// Wikilink targets are project-root-relative, so `page_path` only matters
/// for the (never-delinked) same-project form.
#[must_use]
pub fn delink_wikilinks(
    body: &str,
    page_path: &PagePath,
    delink: &BTreeSet<(Option<String>, Option<String>, String)>,
) -> String {
    if delink.is_empty() {
        return body.to_string();
    }
    let mut out = String::with_capacity(body.len());
    let mut fence: Option<CodeFence> = None;
    for raw_line in body.split_inclusive('\n') {
        let (line, terminator) = split_line_terminator(raw_line);
        let (next_fence, is_fence_or_code) = CodeFence::step(fence, line);
        fence = next_fence;
        if is_fence_or_code {
            out.push_str(line);
            out.push_str(terminator);
            continue;
        }
        let code_spans = inline_code_spans(line);
        let mut pos = 0;
        for span in &code_spans {
            delink_wikilinks_in_segment(&line[pos..span.start], page_path, delink, &mut out);
            out.push_str(&line[span.clone()]);
            pos = span.end;
        }
        delink_wikilinks_in_segment(&line[pos..], page_path, delink, &mut out);
        out.push_str(terminator);
    }
    out
}

fn delink_wikilinks_in_segment(
    seg: &str,
    page_path: &PagePath,
    delink: &BTreeSet<(Option<String>, Option<String>, String)>,
    out: &mut String,
) {
    let mut rest = seg;
    loop {
        let Some(start) = rest.find("[[") else {
            out.push_str(rest);
            return;
        };
        out.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find("]]") else {
            out.push_str(&rest[start..]);
            return;
        };
        let raw = &after_start[..end];
        // Parse exactly as `extract_wikilinks` does so a key in `delink`
        // (built from `extract_links` output) hits the same occurrence.
        let (target_part, label) = raw
            .split_once('|')
            .map_or((raw, None), |(t, l)| (t, Some(l)));
        let unlabelled = target_part.trim();
        let (workspace, project, path_part) = split_scope(unlabelled);
        let delinked = normalize_link_target(&path_part, page_path, true)
            .is_some_and(|path| delink.contains(&(workspace, project, path)));
        if delinked {
            let text = label
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .unwrap_or(unlabelled);
            out.push_str(text);
        } else {
            out.push_str("[[");
            out.push_str(raw);
            out.push_str("]]");
        }
        rest = &after_start[end + 2..];
    }
}

/// Normalise a bare same-project path exactly as [`extract_links`]
/// normalises a wikilink target: trimmed, an extension-less final
/// segment gains `.md`, a non-`.md` extension and a target that cannot
/// name a page are rejected, and `.`/`..` resolve from the wiki root (a
/// wikilink target is root-relative whatever page it sits on, so no
/// source page is needed). The consolidation batch path keys its
/// proposed-to-final redirects with this so a sibling wikilink and the
/// proposal it points at normalise to one key (#1943).
pub fn normalize_wikilink_path(raw: &str) -> Option<PagePath> {
    // `page_path` is unused for wikilink targets (root-relative), so a
    // placeholder stands in for the missing source page.
    let placeholder = PagePath::new("placeholder.md").ok()?;
    let normalized = normalize_link_target(raw, &placeholder, true)?;
    PagePath::new(normalized).ok()
}

/// Rewrite `body`'s same-project wikilinks whose normalised path is a key
/// in `redirects`, replacing the target with the mapped path and keeping
/// any `|label`. Scope-qualified links, fenced code blocks and inline
/// code spans are copied byte-identically, exactly as [`delink_wikilinks`]
/// treats them, and so is every link whose target is not in the map.
///
/// The consolidation batch path uses this to repoint sibling links at a
/// Rule page's final path after the `_rules/<slug>` routing or the
/// same-batch collision resolver moved it away from the path the model
/// proposed (#1943): left alone, those links target a page that never
/// exists, and same-project links are by design allowed to dangle, so no
/// later pass repairs them.
#[must_use]
pub fn rewrite_wikilink_targets(
    body: &str,
    page_path: &PagePath,
    redirects: &BTreeMap<String, String>,
) -> String {
    if redirects.is_empty() {
        return body.to_string();
    }
    let mut out = String::with_capacity(body.len());
    let mut fence: Option<CodeFence> = None;
    for raw_line in body.split_inclusive('\n') {
        let (line, terminator) = split_line_terminator(raw_line);
        let (next_fence, is_fence_or_code) = CodeFence::step(fence, line);
        fence = next_fence;
        if is_fence_or_code {
            out.push_str(line);
            out.push_str(terminator);
            continue;
        }
        let code_spans = inline_code_spans(line);
        let mut pos = 0;
        for span in &code_spans {
            rewrite_wikilink_targets_in_segment(
                &line[pos..span.start],
                page_path,
                redirects,
                &mut out,
            );
            out.push_str(&line[span.clone()]);
            pos = span.end;
        }
        rewrite_wikilink_targets_in_segment(&line[pos..], page_path, redirects, &mut out);
        out.push_str(terminator);
    }
    out
}

fn rewrite_wikilink_targets_in_segment(
    seg: &str,
    page_path: &PagePath,
    redirects: &BTreeMap<String, String>,
    out: &mut String,
) {
    let mut rest = seg;
    loop {
        let Some(start) = rest.find("[[") else {
            out.push_str(rest);
            return;
        };
        out.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find("]]") else {
            out.push_str(&rest[start..]);
            return;
        };
        let raw = &after_start[..end];
        // Parse exactly as `extract_wikilinks` does, mirroring
        // `delink_wikilinks_in_segment`, so the occurrence matched here is
        // the occurrence the extractor indexed.
        let (target_part, label) = raw
            .split_once('|')
            .map_or((raw, None), |(t, l)| (t, Some(l)));
        let unlabelled = target_part.trim();
        let (workspace, project, path_part) = split_scope(unlabelled);
        let redirected = (workspace.is_none() && project.is_none())
            .then(|| normalize_link_target(&path_part, page_path, true))
            .flatten()
            .and_then(|path| redirects.get(&path).cloned());
        if let Some(target) = redirected {
            out.push_str("[[");
            out.push_str(&target);
            if let Some(label) = label {
                out.push('|');
                out.push_str(label);
            }
            out.push_str("]]");
        } else {
            out.push_str("[[");
            out.push_str(raw);
            out.push_str("]]");
        }
        rest = &after_start[end + 2..];
    }
}

/// Resolve a local `[[...]]` wikilink target into `(bundle-relative href,
/// display label)`. Returns `None` when the target is cross-project,
/// cross-workspace, empty, or otherwise not a page — the caller then keeps
/// the literal `[[...]]`.
fn resolve_local_wikilink(
    raw: &str,
    page_path: &PagePath,
    own_project: &str,
) -> Option<(String, String)> {
    let (target_part, label) = raw
        .split_once('|')
        .map_or((raw, None), |(t, l)| (t, Some(l)));
    let target_part = target_part.trim();
    let (workspace, project, path_part) = split_scope(target_part);
    // `split_scope` pins `[[_global:path]]` to the default workspace. When
    // we are exporting that same reserved project, the target is still a
    // local page even though the workspace is now explicit.
    let exporting_global_home = own_project == GLOBAL_SCOPE_PROJECT
        && project.as_deref() == Some(GLOBAL_SCOPE_PROJECT)
        && workspace.as_deref() == Some(DEFAULT_WORKSPACE_NAME);
    if workspace.is_some() && !exporting_global_home {
        return None; // no cross-workspace Markdown equivalent
    }
    if let Some(project) = &project
        && project != own_project
    {
        return None; // no cross-project Markdown equivalent
    }
    // Resolve exactly as `extract_wikilinks` would: root-project-relative
    // (wikilink targets are project-root-relative, not relative to the
    // source page — `page_path` is unused on this branch of
    // `normalize_link_target`/`resolve_relative`), `.md` appended,
    // traversal collapsed.
    // Split off a trailing #anchor/?query so it survives the rewrite.
    let (clean_path, suffix) = match path_part.find(['#', '?']) {
        Some(i) => (&path_part[..i], &path_part[i..]),
        None => (path_part.as_str(), ""),
    };
    let target = normalize_link_target(clean_path, page_path, true)?;
    let page_dir: Vec<&str> = page_path
        .as_str()
        .rsplit_once('/')
        .map_or_else(Vec::new, |(dir, _)| {
            dir.split('/').filter(|s| !s.is_empty()).collect()
        });
    // The wikilink names a literal path; a Markdown destination is a URL
    // that `extract_links` decodes once, so a literal `%` leaves as `%25`.
    let mut href = relative_href(&page_dir, &target).replace('%', "%25");
    href.push_str(suffix);
    let display = label
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .unwrap_or(target_part)
        .to_string();
    Some((href, display))
}

/// Compute a relative path from `page_dir` (a page's own directory,
/// components only, no filename) to `target` (a root-project-relative
/// page path, e.g. `decisions/b.md`), the way a `.md` link on disk in the
/// exported bundle needs it.
fn relative_href(page_dir: &[&str], target: &str) -> String {
    let target_components: Vec<&str> = target.split('/').collect();
    let target_dir = &target_components[..target_components.len().saturating_sub(1)];
    let common = page_dir
        .iter()
        .zip(target_dir.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let ups = page_dir.len() - common;
    let mut parts: Vec<&str> = Vec::with_capacity(ups + target_components.len() - common);
    parts.extend(std::iter::repeat_n("..", ups));
    parts.extend(&target_components[common..]);
    parts.join("/")
}

/// `href` as a Markdown link destination, or `None` when it has none.
///
/// A bare destination cannot hold a space or control character, cannot
/// start with `<`, and needs balanced parentheses; anything else goes
/// inside `<…>` (CommonMark §4.7). A `<` or `>` in a destination that
/// needs the brackets can only be written with a backslash escape that
/// [`extract_links`] does not read back, so that target keeps its literal
/// `[[wikilink]]` rather than export a link that indexes elsewhere.
fn markdown_destination(href: &str) -> Option<String> {
    let mut depth = 0usize;
    let mut bare = !href.starts_with('<');
    for c in href.chars() {
        match c {
            '(' => depth += 1,
            ')' => match depth.checked_sub(1) {
                Some(open) => depth = open,
                None => bare = false,
            },
            ' ' => bare = false,
            c if c.is_ascii_control() => bare = false,
            _ => {}
        }
    }
    if bare && depth == 0 {
        return Some(href.to_string());
    }
    if href.contains(['<', '>']) {
        return None;
    }
    Some(format!("<{href}>"))
}

/// Escape characters that would prematurely close a Markdown link label
/// (`[<label>](<href>)`): brackets, the backslash, and parentheses — an
/// unescaped `)` inside the label closes the link early.
fn escape_markdown_link_label(label: &str) -> String {
    label
        .replace('\\', r"\\")
        .replace('[', r"\[")
        .replace(']', r"\]")
        .replace('(', r"\(")
        .replace(')', r"\)")
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
    // Track labels in one pass: rescanning an unmatched outer label would
    // make repeated `[` quadratic and hide valid inner links.
    let mut labels = Vec::new();
    let mut last_escaped = None;
    let mut pos = 0;
    while pos < line.len() {
        match line.as_bytes()[pos] {
            b'\\' => {
                pos += 1;
                if let Some(next) = line[pos..].chars().next() {
                    last_escaped = Some(pos);
                    pos += next.len_utf8();
                }
            }
            b'[' => {
                labels.push(
                    pos > 0 && line.as_bytes()[pos - 1] == b'!' && last_escaped != Some(pos - 1),
                );
                pos += 1;
            }
            b']' => {
                let image = labels.pop();
                pos += 1;
                if let Some(image) = image
                    && line.as_bytes().get(pos) == Some(&b'(')
                    && let Some((raw, target_end)) = parse_link_destination(&line[pos + 1..])
                {
                    if !image && let Some(path) = normalize_link_target(raw, page_path, false) {
                        out.insert((None, None, path));
                    }
                    pos += target_end + 2;
                    // Images may appear in a link's label; links may not.
                    if !image {
                        labels.clear();
                    }
                }
            }
            _ => pos += 1,
        }
    }
}

/// Longest destination (title included) worth scanning. A page path cannot
/// exceed the filesystem's limits, and an unbounded scan made a line of
/// `[a](` repeated quadratic on the page-write path.
const MAX_LINK_DESTINATION_BYTES: usize = 512;

/// How many whitespace runs in a bare destination are tried as the start of a
/// title before the rest is read as part of the destination.
const MAX_TITLE_PROBES: usize = 8;

/// Parse a CommonMark link destination immediately following the opening `(`
/// of `[label](<destination>)` or `[label](destination)` (CommonMark §4.7),
/// stepping over an optional title.
///
/// Returns `(destination_str, closing_paren_byte_offset)`.
///
/// Two deliberate departures from the spec: a bare destination may hold
/// whitespace when no title follows it (`[x](my page.md)`), which is what
/// earlier versions indexed and what the wikilink export used to emit; and
/// the scan gives up after [`MAX_LINK_DESTINATION_BYTES`].
fn parse_link_destination(rest: &str) -> Option<(&str, usize)> {
    let rest = &rest[..rest.floor_char_boundary(MAX_LINK_DESTINATION_BYTES)];
    let trimmed = rest.trim_start();
    let leading = rest.len() - trimmed.len();
    if let Some(after_lt) = trimmed.strip_prefix('<') {
        let rel_gt = after_lt.find('>')?;
        let after_gt = &after_lt[rel_gt + 1..];
        let rel_paren = closing_paren_after_destination(after_gt)?;
        Some((&after_lt[..rel_gt], leading + 1 + rel_gt + 1 + rel_paren))
    } else {
        let mut depth = 0usize;
        let mut probes = 0;
        let mut prev_ws = false;
        for (i, c) in trimmed.char_indices() {
            let ws = c.is_ascii_whitespace();
            match c {
                '(' => depth += 1,
                ')' if depth == 0 => return Some((trimmed[..i].trim_end(), leading + i)),
                ')' => depth -= 1,
                _ if ws && !prev_ws && depth == 0 && probes < MAX_TITLE_PROBES => {
                    if let Some(close) = closing_paren_after_destination(&trimmed[i..]) {
                        return Some((&trimmed[..i], leading + i + close));
                    }
                    probes += 1;
                }
                _ => {}
            }
            prev_ws = ws;
        }
        None
    }
}

/// Byte offset in `tail` of the `)` that closes a link whose destination
/// ended just before `tail`: optional whitespace, then optionally a title
/// (`"…"`, `'…'` or `(…)`, separated from the destination by whitespace),
/// then the `)`. `None` when `tail` is anything else, so a `)` or a link
/// inside a title is never taken for the end of the link.
fn closing_paren_after_destination(tail: &str) -> Option<usize> {
    let body = tail.trim_start();
    let lead = tail.len() - body.len();
    let mut chars = body.char_indices();
    let (_, first) = chars.next()?;
    if first == ')' {
        return Some(lead);
    }
    let closer = match first {
        _ if lead == 0 => return None,
        '"' => '"',
        '\'' => '\'',
        '(' => ')',
        _ => return None,
    };
    let mut escaped = false;
    for (i, c) in chars {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == closer {
            let after = &body[i + 1..];
            let rest = after.trim_start();
            return rest
                .starts_with(')')
                .then(|| lead + i + 1 + (after.len() - rest.len()));
        } else if first == '(' && c == '(' {
            return None; // an unescaped `(` cannot appear in a `(…)` title
        }
    }
    None
}

/// Decode the path portion of an ordinary Markdown URL destination once.
///
/// Split literal query and fragment delimiters before calling this helper:
/// an encoded `%23` or `%3F` names a character in the filename. Wikilinks
/// name literal page paths and must not go through URL decoding. Invalid
/// escape spellings stay literal; invalid UTF-8 and encoded control bytes
/// have no portable page-path representation and are refused.
#[must_use]
pub fn decode_markdown_destination_path(path: &str) -> Option<std::borrow::Cow<'_, str>> {
    if !path.contains('%') {
        return Some(std::borrow::Cow::Borrowed(path));
    }
    let mut decoded = Vec::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        if bytes[pos] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes
                    .get(pos + 1)
                    .copied()
                    .and_then(|b| char::from(b).to_digit(16)),
                bytes
                    .get(pos + 2)
                    .copied()
                    .and_then(|b| char::from(b).to_digit(16)),
            )
        {
            let byte = (hi * 16 + lo) as u8;
            if byte.is_ascii_control() {
                return None;
            }
            decoded.push(byte);
            pos += 3;
        } else {
            decoded.push(bytes[pos]);
            pos += 1;
        }
    }
    String::from_utf8(decoded).ok().map(std::borrow::Cow::Owned)
}

fn normalize_link_target(raw: &str, page_path: &PagePath, wikilink: bool) -> Option<String> {
    let target = raw
        .split_once('|')
        .map_or(raw, |(path, _)| path)
        .trim()
        .trim_matches('<')
        .trim_matches('>');
    if target.is_empty() || target.starts_with('#') {
        return None;
    }
    let path = target.split(['#', '?']).next()?.trim();
    let decoded = if wikilink {
        std::borrow::Cow::Borrowed(path)
    } else {
        decode_markdown_destination_path(path)?
    };
    let target = decoded.as_ref();
    let lower = target.to_ascii_lowercase();
    if target.is_empty()
        || target.contains("://")
        // A `//host/path` destination is protocol-relative: it leaves the
        // wiki for `host`, so it names no local page.
        || (!wikilink && target.starts_with("//"))
        || target.contains('\\')
        || lower.starts_with("mailto:")
        || lower.starts_with("data:")
        || lower.starts_with("javascript:")
        || lower.starts_with("vbscript:")
        || lower.starts_with("tel:")
        || lower.starts_with("file:")
    {
        return None;
    }

    // A directory target (`notes/`) is not a page: kept, it minted an
    // extension-less `to_path` (or, for a wikilink, the literal
    // `notes/.md`) that no page write could ever resolve.
    if !last_segment_names_a_page(target) {
        return None;
    }

    let mut target = target.to_string();
    let last_segment = target.rsplit_once('/').map_or(target.as_str(), |(_, s)| s);
    if last_segment.contains('.') {
        if !target.ends_with(".md") {
            return None;
        }
    } else {
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
    fn extract_relation_links_global_target_pins_default_workspace() {
        let fm = serde_json::json!({
            "relations": {
                "fixes": ["_global:python-env"],
                "contradicts": ["other/_global:notes/x.md"],
            }
        });
        let links = extract_relation_links(&fm);
        let fixes = links
            .iter()
            .find(|l| l.relation == Some(ai_memory_core::Relation::Fixes))
            .unwrap();
        assert_eq!(fixes.workspace.as_deref(), Some(DEFAULT_WORKSPACE_NAME));
        assert_eq!(fixes.project.as_deref(), Some(GLOBAL_SCOPE_PROJECT));
        assert_eq!(fixes.path.as_str(), "python-env.md");
        let contra = links
            .iter()
            .find(|l| l.relation == Some(ai_memory_core::Relation::Contradicts))
            .unwrap();
        assert_eq!(contra.workspace.as_deref(), Some("other"));
        assert_eq!(contra.project.as_deref(), Some(GLOBAL_SCOPE_PROJECT));
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
    fn directory_and_stemless_targets_are_not_pages() {
        // `sessions/` used to normalize to the literal `sessions/.md` — a
        // link no page write could ever resolve; the same trailing-slash
        // form in the body stayed extension-less, which cannot match a page
        // path either. Both are dropped now.
        let fm = serde_json::json!({
            "relations": {"fixes": ["sessions/", "sessions/.md"]}
        });
        assert!(extract_relation_links(&fm).is_empty());

        assert!(extract_links("- [notes/](notes/)\n", &page()).is_empty());
        assert!(extract_links("- [[notes/]]\n", &page()).is_empty());
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

    /// `[[_global:path]]` is a project-only qualifier, but the reserved
    /// preferences project lives in the default workspace (#1042). Pinning
    /// the workspace here is what lets a page outside `default` resolve.
    #[test]
    fn extract_links_global_wikilink_pins_default_workspace() {
        let links = extract_links(
            "see [[_global:python-env]] and [[_global:local-machine.md|env]]",
            &page(),
        );
        let pinned: Vec<_> = links
            .iter()
            .filter(|l| l.project.as_deref() == Some(GLOBAL_SCOPE_PROJECT))
            .collect();
        assert_eq!(pinned.len(), 2, "{links:?}");
        assert!(
            pinned
                .iter()
                .all(|l| l.workspace.as_deref() == Some(DEFAULT_WORKSPACE_NAME)),
            "project-only _global must name the default workspace: {links:?}"
        );
        assert!(pinned.iter().any(|l| l.path.as_str() == "python-env.md"));
        assert!(pinned.iter().any(|l| l.path.as_str() == "local-machine.md"));

        let sibling = extract_links("[[infra:runbooks/02.md]]", &page());
        let l = sibling
            .iter()
            .find(|l| l.is_cross_project())
            .expect("infra");
        assert_eq!(
            l.workspace, None,
            "non-global project-only stays source-workspace"
        );
        assert_eq!(l.project.as_deref(), Some("infra"));

        let explicit = extract_links("[[other/_global:notes/x.md]]", &page());
        let l = explicit
            .iter()
            .find(|l| l.is_cross_project())
            .expect("explicit");
        assert_eq!(
            l.workspace.as_deref(),
            Some("other"),
            "an explicit workspace is kept"
        );
        assert_eq!(l.project.as_deref(), Some(GLOBAL_SCOPE_PROJECT));
    }

    #[test]
    fn extract_links_url_wikilink_is_not_a_scope() {
        // `https://...` must not be parsed as project "https".
        let links = extract_links("[[https://example.com]] [[mailto:a@b.com]]", &page());
        assert!(links.is_empty(), "URLs/schemes are not links: {links:?}");
    }

    #[test]
    fn extract_links_protocol_relative_destination_is_external() {
        // `//host/path` is a network-path reference (RFC 3986 §4.2): a
        // renderer sends the reader to `host`, so it is no wiki page.
        let links = extract_links("[x](//example.com/guide.md) [y](/notes/a.md)", &page());
        let paths: Vec<_> = links.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(paths, ["notes/a.md"], "{links:?}");
    }

    #[test]
    fn extract_links_file_and_vbscript_schemes_are_not_scopes() {
        // `/web` already leaves these literal. Without the same prefixes
        // here, `[[file:notes/x.md]]` peels `file` as a project scope and
        // indexes a cross-project edge no page write can satisfy.
        let links = extract_links(
            "[[file:notes/x.md]] [[vbscript:msgbox]] [[file:///etc/passwd]]",
            &page(),
        );
        assert!(
            links.is_empty(),
            "file/vbscript schemes are not wiki links: {links:?}"
        );
        let md = extract_links("[x](file:notes/x.md) [y](vbscript:msgbox)", &page());
        assert!(
            md.is_empty(),
            "file/vbscript markdown destinations are not wiki links: {md:?}"
        );
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

    /// A page saved with CRLF line endings (a Windows editor, or a wiki
    /// checked out with `core.autocrlf=true`): the fence lines end in
    /// `\r\n`, but they are still the canonical frontmatter block. Treating
    /// the file as body-only drops `pinned`/`tier`/`expires_at` on reindex
    /// and lets the OKF file pass write a second frontmatter block above
    /// the first.
    #[test]
    fn parses_crlf_frontmatter() {
        let src = "---\r\ntitle: Hello\r\npinned: true\r\ntags:\r\n  - a\r\n---\r\nBody\r\n";
        let md = parse(src).unwrap();
        assert_eq!(md.frontmatter["title"], "Hello");
        assert_eq!(md.frontmatter["pinned"], true);
        assert_eq!(md.frontmatter["tags"][0], "a");
        assert_eq!(md.body, "Body\r\n");
    }

    /// A frontmatter-only file whose closing fence is the last line, saved
    /// without a final newline (the editor default in several editors). The
    /// block is still the frontmatter: reading it as body loses the `_meta.md`
    /// workspace name and a page's tier, pin and TTL, and the OKF file pass
    /// would write a second frontmatter block above it.
    #[test]
    fn parses_frontmatter_closed_at_end_of_file() {
        let md = parse("---\ntitle: Hello\npinned: true\n---").unwrap();
        assert_eq!(md.frontmatter["title"], "Hello");
        assert_eq!(md.frontmatter["pinned"], true);
        assert_eq!(md.body, "");
        assert_eq!(
            emit(&md).unwrap(),
            "---\ntitle: Hello\npinned: true\n---\n",
            "re-emitting must keep a single frontmatter block"
        );

        let crlf = parse("\u{FEFF}---\r\ntitle: Hello\r\n---").unwrap();
        assert_eq!(crlf.frontmatter["title"], "Hello");
        assert_eq!(crlf.body, "");
    }

    /// An empty block (`---` straight after `---`, the Jekyll idiom) is
    /// frontmatter. Its closing fence has no newline in front of it, so it
    /// was missed: the page body was read as YAML up to the next `---` rule,
    /// or the whole file was kept as body.
    #[test]
    fn parses_empty_frontmatter_block() {
        let md = parse("---\n---\n# Title\n\nIntro.\n\n---\n\nMore.\n").unwrap();
        assert!(md.frontmatter.is_null(), "{:?}", md.frontmatter);
        assert_eq!(md.body, "# Title\n\nIntro.\n\n---\n\nMore.\n");

        let md = parse("---\n---\nbody").unwrap();
        assert!(md.frontmatter.is_null());
        assert_eq!(md.body, "body");

        let crlf = parse("\u{FEFF}---\r\n---\r\nbody\r\n").unwrap();
        assert!(crlf.frontmatter.is_null());
        assert_eq!(crlf.body, "body\r\n");

        let only = parse("---\n---").unwrap();
        assert!(only.frontmatter.is_null());
        assert_eq!(only.body, "");
    }

    /// A body line that merely starts with `---` at the end of the file is
    /// not a closing fence.
    #[test]
    fn trailing_dashes_line_is_not_a_closing_fence() {
        let src = "---\ntitle: Hello\n----";
        let md = parse(src).unwrap();
        assert!(md.frontmatter.is_null());
        assert_eq!(md.body, src);
    }

    /// A page a Windows editor saved with a UTF-8 BOM and no frontmatter:
    /// the mark belongs to the file, not to the first line. Left in `body`
    /// it sits in front of the `#`, so the H1 stops being a heading and the
    /// page is indexed under its filename instead of its title.
    #[test]
    fn parses_bom_prefixed_body_without_frontmatter() {
        let src = "\u{FEFF}# Hand written\n\nBody.\n";
        let md = parse(src).unwrap();
        assert!(md.frontmatter.is_null());
        assert_eq!(md.body, "# Hand written\n\nBody.\n");
        assert_eq!(
            derive_title(
                &md.frontmatter,
                &md.body,
                &PagePath::new("notes/hand-written.md").unwrap(),
            ),
            "Hand written"
        );
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

    /// Shapes serde_yaml 0.9 (libyaml, YAML 1.1) read and granit-parser
    /// (YAML 1.2) reads otherwise. Pinned so a parser bump that changes one
    /// is a red test, not a page that silently stops indexing.
    #[test]
    fn frontmatter_shapes_read_differently_since_serde_saphyr() {
        let refused = |yaml: &str| {
            let err = parse(&format!("---\n{yaml}\n---\nbody\n")).unwrap_err();
            assert!(
                matches!(err, crate::error::WikiError::Yaml(_)),
                "{yaml:?}: {err:?}"
            );
        };
        // A comment glued to a closing quote or a block indicator.
        refused("title: 'Release notes'# reviewed");
        refused("a: \"x\"# c");
        refused(">#x");
        // U+2028 was a line break under YAML 1.1.
        refused(">+\u{2028}");
        refused("a: x\u{2028}b: y");
        // Kept as content now, folded to a space before.
        assert_eq!(
            frontmatter_of("a: \"x\u{85}y\""),
            serde_json::json!({ "a": "x\u{85}y" })
        );
        assert_eq!(
            frontmatter_of("a: |\n  x\u{2028}  y"),
            serde_json::json!({ "a": "x\u{2028}  y" })
        );
        // Control: with a space before `#`, both parsers agree.
        assert_eq!(
            frontmatter_of("title: 'Release notes' # reviewed"),
            serde_json::json!({ "title": "Release notes" })
        );
    }

    fn frontmatter_of(yaml: &str) -> serde_json::Value {
        parse(&format!("---\n{yaml}\n---\nbody\n"))
            .unwrap()
            .frontmatter
    }

    /// What `serde_yaml` 0.9 indexed for each value spelling (`None`: it
    /// refused the page). A hand-edited page must index the same now.
    #[test]
    fn frontmatter_values_match_serde_yaml_0_9() {
        use serde_json::json;
        let cases: &[(&str, Option<serde_json::Value>)] = &[
            // Leading zeros, digit separators, upper-case prefixes and
            // out-of-range floats were text.
            ("007", Some(json!("007"))),
            ("+08", Some(json!("+08"))),
            ("-01", Some(json!("-01"))),
            ("1_000", Some(json!("1_000"))),
            ("0X1F", Some(json!("0X1F"))),
            ("0O17", Some(json!("0O17"))),
            ("1e999", Some(json!("1e999"))),
            // Infinity and NaN have no JSON form; serde_json wrote null.
            (".inf", Some(json!(null))),
            ("-.inf", Some(json!(null))),
            (".nan", Some(json!(null))),
            ("+.nan", Some(json!("+.nan"))),
            // Integers wider than 64 bits were refused.
            ("18446744073709551616", None),
            ("-18446744073709551616", None),
            (
                "18446744073709551615",
                Some(json!(18_446_744_073_709_551_615_u64)),
            ),
            ("-9223372036854775808", Some(json!(i64::MIN))),
            // A local tag became a one-entry map; core tags kept the text.
            ("!foo bar", Some(json!({ "!foo": "bar" }))),
            ("!foo [1, 007]", Some(json!({ "!foo": [1, "007"] }))),
            ("!!timestamp 2026-10-12", Some(json!("2026-10-12"))),
            ("!!str 123", Some(json!("123"))),
            ("!!binary aGVsbG8=", Some(json!("aGVsbG8="))),
            // Controls.
            ("yes", Some(json!("yes"))),
            ("on", Some(json!("on"))),
            ("True", Some(json!(true))),
            ("2026-10-12", Some(json!("2026-10-12"))),
            ("2026-10-12T01:08:40Z", Some(json!("2026-10-12T01:08:40Z"))),
            ("0.92", Some(json!(0.92))),
            ("1e3", Some(json!(1000.0))),
            ("0x1F", Some(json!(31))),
            ("0o17", Some(json!(15))),
            ("0b101", Some(json!(5))),
            ("1:20", Some(json!("1:20"))),
            ("~", Some(json!(null))),
            ("", Some(json!(null))),
            ("'123'", Some(json!("123"))),
        ];
        for (yaml, expected) in cases {
            let parsed = parse(&format!("---\nv: {yaml}\n---\nbody\n"));
            match expected {
                Some(value) => assert_eq!(
                    parsed.map(|md| md.frontmatter).ok(),
                    Some(json!({ "v": value })),
                    "v: {yaml}"
                ),
                None => assert!(
                    matches!(parsed, Err(crate::error::WikiError::Yaml(_))),
                    "v: {yaml} must be refused, got {parsed:?}"
                ),
            }
        }
    }

    #[test]
    fn last_block_scalar_drops_its_final_newline_as_serde_yaml_did() {
        // The fence drops the newline before `---`, so the YAML ends without
        // one; libyaml did not count the missing break into the block.
        let fm = frontmatter_of("a: |\n  kept\nb: |\n  last");
        assert_eq!(fm, serde_json::json!({ "a": "kept\n", "b": "last" }));
        let fm = frontmatter_of("b: |\n  last\n ");
        assert_eq!(fm, serde_json::json!({ "b": "last\n" }));
    }

    #[test]
    fn empty_frontmatter_block_is_null() {
        let md = parse("---\n\n---\nbody\n").unwrap();
        assert!(md.frontmatter.is_null());
        assert_eq!(md.body, "body\n");
    }

    #[test]
    fn duplicate_frontmatter_keys_are_rejected() {
        let err = parse("---\ntitle: a\ntitle: b\n---\nbody\n").unwrap_err();
        let crate::error::WikiError::Yaml(msg) = err else {
            panic!("expected WikiError::Yaml, got {err:?}");
        };
        assert!(msg.contains("duplicate"), "{msg}");
        assert!(msg.contains("line 2"), "error names the location: {msg}");
    }

    #[test]
    fn block_scalars_and_multiline_strings_round_trip() {
        let fm =
            frontmatter_of("lit: |\n  one\n  two\nfold: >\n  folded\n  text\nstrip: |-\n  keep");
        assert_eq!(
            fm,
            serde_json::json!({
                "lit": "one\ntwo\n",
                "fold": "folded text\n",
                "strip": "keep",
            })
        );
        let emitted = emit(&Markdown {
            frontmatter: fm.clone(),
            body: String::new(),
        })
        .unwrap();
        // serde_yaml 0.9 wrote newline-bearing strings as literal blocks.
        assert_eq!(
            emitted,
            "---\nlit: |\n  one\n  two\nfold: |\n  folded text\nstrip: keep\n---\n"
        );
        assert_eq!(parse(&emitted).unwrap().frontmatter, fm);
    }

    #[test]
    fn keys_and_values_that_need_quotes_round_trip() {
        let fm = serde_json::json!({
            "key: colon": "a # b",
            "#hash": "@at",
            "123": "true",
            "": "null",
            "plain": "2026-10-12",
        });
        let emitted = emit(&Markdown {
            frontmatter: fm.clone(),
            body: String::new(),
        })
        .unwrap();
        // Quoting uses double quotes (serde_yaml 0.9 used single quotes);
        // dates stay plain, as serde_yaml wrote them.
        assert_eq!(
            emitted,
            "---\n\"key: colon\": \"a # b\"\n\"#hash\": \"@at\"\n\"123\": \"true\"\n\"\": \"null\"\nplain: 2026-10-12\n---\n"
        );
        assert_eq!(parse(&emitted).unwrap().frontmatter, fm);
    }

    #[test]
    fn non_ascii_and_long_strings_emit_plain_on_one_line() {
        let long = "word ".repeat(400).trim_end().to_string();
        let fm = serde_json::json!({ "title": "Olá — 日本語 🎉", "long": long });
        let emitted = emit(&Markdown {
            frontmatter: fm.clone(),
            body: String::new(),
        })
        .unwrap();
        assert_eq!(
            emitted,
            format!("---\ntitle: Olá — 日本語 🎉\nlong: {long}\n---\n")
        );
        assert_eq!(parse(&emitted).unwrap().frontmatter, fm);
    }

    /// A page in the shape serde_yaml 0.9 wrote (compact list indent, plain
    /// timestamps, `{}`/`[]` for empty collections) re-emits byte for byte,
    /// so the parser swap rewrites nothing on a page's next save.
    #[test]
    fn serde_yaml_shaped_page_re_emits_byte_for_byte() {
        let src = "---\n\
            kind: decision\n\
            confidence: 0.92\n\
            pinned: false\n\
            created_at: 2026-10-12T01:08:40Z\n\
            status: yes\n\
            tags:\n\
            - rust\n\
            - 日本語\n\
            summary: |\n  First line.\n  Second line.\n\
            evidence:\n\
            - kind: observation\n  id: 019e5633-4140-7dd0-9e2f-118825ca6355\n  at: 2026-07-18T09:35:52Z\n\
            - kind: page\n  path: decisions/0001.md\n\
            relations: {}\n\
            aliases: []\n\
            ---\n\
            # Body\n";
        assert_eq!(emit(&parse(src).unwrap()).unwrap(), src);
    }

    #[test]
    fn frontmatter_alias_bomb_is_refused_quickly() {
        // Nine levels of nine-fold aliases expand to 9^9 nodes.
        let mut yaml = String::from("a: &a [x, x, x, x, x, x, x, x, x]\n");
        let mut prev = 'a';
        for name in "bcdefghi".chars() {
            let refs = vec![format!("*{prev}"); 9].join(", ");
            yaml.push_str(&format!("{name}: &{name} [{refs}]\n"));
            prev = name;
        }
        let started = std::time::Instant::now();
        let err = parse(&format!("---\n{yaml}---\nbody\n")).unwrap_err();
        assert!(
            matches!(err, crate::error::WikiError::Yaml(_)),
            "alias bomb must surface as a YAML error: {err:?}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "alias bomb refusal must stay bounded, took {:?}",
            started.elapsed()
        );

        // Control: ordinary anchors and aliases still parse, as before.
        let fm = frontmatter_of("base: &b\n  tier: core\nref: *b\nlist: [*b]\nmerged:\n  <<: *b");
        assert_eq!(
            fm,
            serde_json::json!({
                "base": { "tier": "core" },
                "ref": { "tier": "core" },
                "list": [{ "tier": "core" }],
                // serde_yaml 0.9 kept `<<` as an ordinary key.
                "merged": { "<<": { "tier": "core" } },
            })
        );
    }

    #[test]
    fn frontmatter_nesting_is_capped_at_serde_yaml_depth() {
        let nested = |depth: usize, inner: &str| {
            format!("{}{inner}{}", "[".repeat(depth), "]".repeat(depth))
        };
        // serde_yaml allowed 128 nested collections, the top mapping included.
        assert!(parse(&format!("---\nv: {}\n---\n", nested(127, "1"))).is_ok());
        let err = parse(&format!("---\nv: {}\n---\n", nested(128, "1"))).unwrap_err();
        assert!(matches!(err, crate::error::WikiError::Yaml(_)), "{err:?}");
        // An alias counts the depth it lands at, as serde_yaml's replay did.
        let deep_alias = format!("a: &a {}\nb: {}", nested(100, "1"), nested(30, "*a"));
        let err = parse(&format!("---\n{deep_alias}\n---\n")).unwrap_err();
        assert!(matches!(err, crate::error::WikiError::Yaml(_)), "{err:?}");
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

    /// Agent-written pages often open with a fenced command block. A
    /// `# comment` inside it is code, not the page's H1, so it must not
    /// become the indexed title.
    #[test]
    fn title_skips_comment_lines_inside_code_fences() {
        let path = PagePath::new("notes/setup.md").unwrap();
        let null = serde_json::Value::Null;
        assert_eq!(
            derive_title(
                &null,
                "```bash\n# install deps\nnpm i\n```\n# Real Title\n",
                &path
            ),
            "Real Title"
        );
        assert_eq!(
            derive_title(&null, "~~~python\n# compute\nx = 1\n~~~\nbody\n", &path),
            "setup",
            "a fenced comment must not replace the stem fallback"
        );
        // An unclosed fence runs to the end of the body.
        assert_eq!(derive_title(&null, "```sh\n# only code\n", &path), "setup");
        // A heading after an inline triple-backtick span is still a heading.
        assert_eq!(
            derive_title(&null, "```inline``` text\n# Heading\n", &path),
            "Heading"
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

    /// Build a `delink` key exactly as the consolidation guard does: from
    /// `extract_links` output for the cross-project targets named in
    /// `targets`.
    fn delink_set(body: &str, page: &PagePath, targets: &[&str]) -> BTreeSet<LinkKey> {
        extract_links(body, page)
            .into_iter()
            .filter(|l| l.project.is_some() && targets.contains(&l.path.as_str()))
            .map(|l| (l.workspace, l.project, l.path.as_str().to_string()))
            .collect()
    }

    #[test]
    fn delink_wikilinks_strips_only_listed_cross_scope_links() {
        let page = PagePath::new("notes/a.md").unwrap();
        let body = "See [[infra:runbooks/02.md]] and [[infra:runbooks/missing.md|the gap]], \
                    plus [[notes/pending.md]] and [[infra:runbooks/02.md|again]].\n";
        let delink = delink_set(body, &page, &["runbooks/missing.md"]);
        let out = delink_wikilinks(body, &page, &delink);
        assert_eq!(
            out,
            "See [[infra:runbooks/02.md]] and the gap, \
             plus [[notes/pending.md]] and [[infra:runbooks/02.md|again]].\n",
            "only the unresolved link is de-linked; the resolved one and the \
             same-project pending link stay byte-identical"
        );
    }

    #[test]
    fn delink_wikilinks_without_label_keeps_the_target_text() {
        let page = PagePath::new("notes/a.md").unwrap();
        let body = "Depends on [[_global:nope]].\n";
        let delink = delink_set(body, &page, &["nope.md"]);
        assert_eq!(delink.len(), 1, "_global target is extracted");
        assert_eq!(
            delink_wikilinks(body, &page, &delink),
            "Depends on _global:nope.\n"
        );
    }

    #[test]
    fn delink_wikilinks_skips_code_and_leaves_empty_set_untouched() {
        let page = PagePath::new("notes/a.md").unwrap();
        let body = "```\n[[infra:runbooks/missing.md]]\n```\n\
                    Inline `[[infra:runbooks/missing.md]]` stays code, \
                    but [[infra:runbooks/missing.md]] is prose.\n";
        let delink = delink_set(body, &page, &["runbooks/missing.md"]);
        let out = delink_wikilinks(body, &page, &delink);
        assert_eq!(
            out,
            "```\n[[infra:runbooks/missing.md]]\n```\n\
             Inline `[[infra:runbooks/missing.md]]` stays code, \
             but infra:runbooks/missing.md is prose.\n"
        );
        // An empty set must be a byte-identical no-op.
        assert_eq!(delink_wikilinks(body, &page, &BTreeSet::new()), body);
    }

    /// #1943: the redirect keys normalise exactly as a wikilink the
    /// extractor reads, whatever the model's proposed path looked like.
    #[test]
    fn normalize_wikilink_path_matches_the_extractor_fold() {
        for raw in ["_rules/acme", "_rules/acme.md"] {
            assert_eq!(
                normalize_wikilink_path(raw).map(|p| p.as_str().to_owned()),
                Some("_rules/acme.md".to_owned()),
                "{raw:?}"
            );
        }
        // A non-md extension, a directory and an escaping target are not
        // pages, and neither is anything above the root.
        assert_eq!(normalize_wikilink_path("notes/a.txt"), None);
        assert_eq!(normalize_wikilink_path("notes/"), None);
        assert_eq!(normalize_wikilink_path("../escape.md"), None);
        // Whatever page a link sits on, a wikilink target is root-relative.
        for source in ["notes/a.md", "deep/dir/b.md"] {
            let body = "See [[_rules/acme]].";
            let from = extract_links(body, &PagePath::new(source).unwrap());
            assert_eq!(from[0].path.as_str(), "_rules/acme.md");
        }
    }

    /// #1943: only same-project links whose normalised target is a key in
    /// the map move; labels, code, scope-qualified links and unmatched
    /// targets are copied byte-identically.
    #[test]
    fn rewrite_wikilink_targets_repoints_only_listed_same_project_links() {
        let page = PagePath::new("sessions/a.md").unwrap();
        let redirects = BTreeMap::from([(
            "_rules/acme.md".to_owned(),
            "_rules/acme-c230fc38.md".to_owned(),
        )]);
        let body = "See [[_rules/acme]], [[_rules/acme|the rule]] and \
                    [[infra:_rules/acme]]; keep [[notes/pending]] too.\n\
                    `[_rules/acme]` stays code.\n";
        let out = rewrite_wikilink_targets(body, &page, &redirects);
        assert_eq!(
            out,
            "See [[_rules/acme-c230fc38.md]], [[_rules/acme-c230fc38.md|the rule]] and \
             [[infra:_rules/acme]]; keep [[notes/pending]] too.\n\
             `[_rules/acme]` stays code.\n"
        );
        // An empty map is a byte-identical no-op.
        assert_eq!(
            rewrite_wikilink_targets(body, &page, &BTreeMap::new()),
            body
        );
    }

    #[test]
    fn extract_links_ignores_inline_code_spans() {
        // The web page renders all of these code spans as code, so a link
        // written inside one is not a link a reader can follow.
        let path = PagePath::new("notes/a.md").unwrap();
        let body = "Write `[[notes/syntax]]` or `[[other-project:notes/x]]` to link.\n\
                    A ``[[notes/double]] `nested` `` span, and `[label](md-link.md)`.\n\
                    See [`the flow`](flow.md) and [[notes/kept]].\n\
                    A lone ` backtick hides nothing: [[notes/after-tick]].\n";
        let links = extract_links(body, &path);
        let targets: Vec<(Option<&str>, &str)> = links
            .iter()
            .map(|l| (l.project.as_deref(), l.path.as_str()))
            .collect();
        assert_eq!(
            targets,
            vec![
                (None, "notes/after-tick.md"),
                (None, "notes/flow.md"),
                (None, "notes/kept.md"),
            ]
        );
    }

    #[test]
    fn rewrite_local_wikilinks_resolves_at_various_depths() {
        // concepts/a.md -> decisions/b.md: up one, then into decisions/.
        let a = PagePath::new("concepts/a.md").unwrap();
        assert_eq!(
            rewrite_local_wikilinks("See [[decisions/b.md]].", &a, "proj"),
            "See [decisions/b.md](../decisions/b.md)."
        );

        // Bundle root -> a nested page: no "../" needed.
        let root = PagePath::new("a.md").unwrap();
        assert_eq!(
            rewrite_local_wikilinks("[[concepts/c.md]]", &root, "proj"),
            "[concepts/c.md](concepts/c.md)"
        );

        // Same directory: no "../" and no shared-prefix repetition.
        assert_eq!(
            rewrite_local_wikilinks("[[concepts/c.md]]", &a, "proj"),
            "[concepts/c.md](c.md)"
        );

        // Two levels deep.
        let nested = PagePath::new("a/b/c.md").unwrap();
        assert_eq!(
            rewrite_local_wikilinks("[[decisions/d.md]]", &nested, "proj"),
            "[decisions/d.md](../../decisions/d.md)"
        );
    }

    #[test]
    fn rewrite_local_wikilinks_preserves_explicit_label() {
        let path = PagePath::new("concepts/a.md").unwrap();
        assert_eq!(
            rewrite_local_wikilinks("[[decisions/b.md|the decision]]", &path, "proj"),
            "[the decision](../decisions/b.md)"
        );
    }

    #[test]
    fn rewrite_local_wikilinks_falls_back_to_target_for_an_empty_label() {
        // An empty or blank `|label` must not export as `[](…)`, a link with
        // no text to click. The web view and `delink_wikilinks` already show
        // the target in that case.
        let path = PagePath::new("concepts/a.md").unwrap();
        assert_eq!(
            rewrite_local_wikilinks("[[decisions/b.md|]]", &path, "proj"),
            "[decisions/b.md](../decisions/b.md)"
        );
        assert_eq!(
            rewrite_local_wikilinks("[[decisions/b|  ]]", &path, "proj"),
            "[decisions/b](../decisions/b.md)"
        );
    }

    /// A wikilink names a literal page path, but the exported Markdown link
    /// is a URL that `extract_links` decodes once. A `%` in the filename has
    /// to leave as `%25`, or the export points at another page (`a%20b.md`
    /// read as `a b.md`) or at none (`95%cap.md` does not decode).
    #[test]
    fn rewrite_local_wikilinks_keeps_a_literal_percent_in_the_target() {
        let page = PagePath::new("notes/here.md").unwrap();
        for (body, href) in [
            ("See [[notes/a%20b]].\n", "a%2520b.md"),
            ("See [[notes/cpu-95%cap]].\n", "cpu-95%25cap.md"),
        ] {
            let before = extract_links(body, &page);
            assert_eq!(before.len(), 1, "{body}");
            let exported = rewrite_local_wikilinks(body, &page, "proj");
            assert!(exported.contains(&format!("]({href})")), "{exported}");
            assert_eq!(extract_links(&exported, &page), before, "{exported}");
        }
    }

    #[test]
    fn rewrite_local_wikilinks_preserves_anchor_and_query_fragments() {
        let path = PagePath::new("concepts/a.md").unwrap();
        assert_eq!(
            rewrite_local_wikilinks("See [[decisions/b.md#context]].", &path, "proj"),
            "See [decisions/b.md#context](../decisions/b.md#context)."
        );
        assert_eq!(
            rewrite_local_wikilinks("[[decisions/b#context|the decision]]", &path, "proj"),
            "[the decision](../decisions/b.md#context)"
        );
        assert_eq!(
            rewrite_local_wikilinks("[[decisions/b.md?v=1#part-2]]", &path, "proj"),
            "[decisions/b.md?v=1#part-2](../decisions/b.md?v=1#part-2)"
        );
        assert_eq!(
            rewrite_local_wikilinks("[[decisions/b#my section|My Section]]", &path, "proj"),
            "[My Section](<../decisions/b.md#my section>)"
        );
    }

    #[test]
    fn rewrite_local_wikilinks_leaves_cross_project_untouched() {
        let path = PagePath::new("concepts/a.md").unwrap();
        assert_eq!(
            rewrite_local_wikilinks("[[other-project:decisions/b.md]]", &path, "proj"),
            "[[other-project:decisions/b.md]]"
        );
        assert_eq!(
            rewrite_local_wikilinks("[[ws/other-project:decisions/b.md]]", &path, "proj"),
            "[[ws/other-project:decisions/b.md]]"
        );
        assert_eq!(
            rewrite_local_wikilinks("[[_global:python-env.md]]", &path, "proj"),
            "[[_global:python-env.md]]"
        );
    }

    #[test]
    fn rewrite_local_wikilinks_resolves_global_when_exporting_global() {
        let path = PagePath::new("concepts/a.md").unwrap();
        assert_eq!(
            rewrite_local_wikilinks("[[_global:python-env.md]]", &path, GLOBAL_SCOPE_PROJECT),
            "[_global:python-env.md](../python-env.md)"
        );
        assert_eq!(
            rewrite_local_wikilinks(
                "[[default/_global:python-env.md]]",
                &path,
                GLOBAL_SCOPE_PROJECT,
            ),
            "[default/_global:python-env.md](../python-env.md)"
        );
    }

    #[test]
    fn rewrite_local_wikilinks_resolves_explicit_own_project_scope() {
        let path = PagePath::new("concepts/a.md").unwrap();
        assert_eq!(
            rewrite_local_wikilinks("[[proj:decisions/b.md]]", &path, "proj"),
            "[proj:decisions/b.md](../decisions/b.md)"
        );
    }

    #[test]
    fn rewrite_local_wikilinks_skips_fenced_and_inline_code() {
        let path = PagePath::new("concepts/a.md").unwrap();
        let body = "```\n[[decisions/ignored.md]]\n```\n\
                    Use `[[decisions/also-ignored.md]]` inline.\n\
                    [[decisions/kept.md]]\n";
        let rewritten = rewrite_local_wikilinks(body, &path, "proj");
        assert!(rewritten.contains("[[decisions/ignored.md]]"));
        assert!(rewritten.contains("`[[decisions/also-ignored.md]]`"));
        assert!(rewritten.contains("[decisions/kept.md](../decisions/kept.md)"));
    }

    #[test]
    fn rewrite_local_wikilinks_leaves_malformed_or_empty_targets_untouched() {
        let path = PagePath::new("concepts/a.md").unwrap();
        for body in ["[[]]", "[[   ]]", "[[unterminated", "no links here at all"] {
            assert_eq!(rewrite_local_wikilinks(body, &path, "proj"), body);
        }
    }

    #[test]
    fn rewrite_local_wikilinks_does_not_panic_on_arbitrary_bodies() {
        let path = PagePath::new("concepts/a.md").unwrap();
        for body in [
            "[[",
            "]]",
            "[[|]]",
            "[[a|b|c]]",
            "``` [[unterminated fence",
            "`[[dangling backtick",
            "[[../../escape.md]]",
        ] {
            let _ = rewrite_local_wikilinks(body, &path, "proj");
        }
    }

    #[test]
    fn code_fence_respects_glyph_and_length() {
        let md = "~~~\n[[a/b.md]]\n```\n[[c/d.md]]\n~~~\nafter [[e/f.md]]\n";
        let path = PagePath::new("concepts/a.md").unwrap();
        let links = extract_links(md, &path);
        assert_eq!(links.len(), 1, "only link outside fence extracted");
        assert_eq!(links[0].path.as_str(), "e/f.md");

        let rewritten = rewrite_local_wikilinks(md, &path, "proj");
        assert!(rewritten.contains("[[a/b.md]]"), "a/b remains literal");
        assert!(rewritten.contains("[[c/d.md]]"), "c/d remains literal");
        assert!(
            rewritten.contains("[e/f.md](../e/f.md)"),
            "post-fence wikilink rewritten: {rewritten}"
        );

        // 4 backticks cannot be closed by 3 backticks
        let md4 = "````\n[[inside4.md]]\n```\n[[still_inside.md]]\n````\nafter [[outside.md]]\n";
        let links4 = extract_links(md4, &path);
        assert_eq!(links4.len(), 1);
        assert_eq!(links4[0].path.as_str(), "outside.md");
    }

    #[test]
    fn indented_pseudo_closers_do_not_expose_or_rewrite_fenced_links() {
        let page = PagePath::new("concepts/a.md").unwrap();
        let delink = BTreeSet::from([
            (
                None,
                Some("infra".to_string()),
                "runbooks/hidden.md".to_string(),
            ),
            (
                None,
                Some("infra".to_string()),
                "runbooks/visible.md".to_string(),
            ),
        ]);

        for prefix in ["    ", "\t", "\u{2003}"] {
            let body = format!(
                "````md\n{prefix}````\n[[decisions/hidden.md]]\n\
                 [[infra:runbooks/hidden.md]]\n````\n[[decisions/visible.md]]\n\
                 [[infra:runbooks/visible.md]]\n"
            );
            let links = extract_links(&body, &page);
            let paths: Vec<&str> = links.iter().map(|link| link.path.as_str()).collect();
            assert_eq!(
                paths,
                ["decisions/visible.md", "runbooks/visible.md"],
                "prefix {prefix:?}"
            );

            let rewritten = rewrite_local_wikilinks(&body, &page, "proj");
            assert!(
                rewritten.contains("[[decisions/hidden.md]]"),
                "prefix {prefix:?}: {rewritten}"
            );
            assert!(
                rewritten.contains("[decisions/visible.md](../decisions/visible.md)"),
                "prefix {prefix:?}: {rewritten}"
            );

            let delinked = delink_wikilinks(&body, &page, &delink);
            assert!(
                delinked.contains("[[infra:runbooks/hidden.md]]"),
                "prefix {prefix:?}: {delinked}"
            );
            assert!(
                delinked.contains("\ninfra:runbooks/visible.md\n"),
                "prefix {prefix:?}: {delinked}"
            );
        }
    }

    #[test]
    fn extract_links_parses_balanced_parentheses_and_pointy_destinations() {
        let root = PagePath::new("here.md").unwrap();
        let md = "See [doc](notes/foo_(1).md), [space](<notes/bar (2).md>), and [title](notes/baz.md \"a title\").\n";
        let links = extract_links(md, &root);
        assert_eq!(links.len(), 3, "{links:?}");
        assert!(links.iter().any(|l| l.path.as_str() == "notes/foo_(1).md"));
        assert!(links.iter().any(|l| l.path.as_str() == "notes/bar (2).md"));
        assert!(links.iter().any(|l| l.path.as_str() == "notes/baz.md"));
    }

    #[test]
    fn rewrite_local_wikilinks_encloses_destinations_with_spaces_in_pointy_brackets() {
        let path = PagePath::new("concepts/a.md").unwrap();
        let rewritten = rewrite_local_wikilinks("See [[decisions/my decision.md]].", &path, "proj");
        assert_eq!(
            rewritten,
            "See [decisions/my decision.md](<../decisions/my decision.md>)."
        );

        // Without spaces, no pointy brackets needed
        let plain = rewrite_local_wikilinks("See [[decisions/b.md]].", &path, "proj");
        assert_eq!(plain, "See [decisions/b.md](../decisions/b.md).");
    }

    /// Link paths `md` yields when it sits at the wiki root, sorted.
    fn linked(md: &str) -> Vec<String> {
        let root = PagePath::new("here.md").unwrap();
        extract_links(md, &root)
            .into_iter()
            .map(|l| l.path.as_str().to_string())
            .collect()
    }

    #[test]
    fn extract_links_keeps_whitespace_in_bare_destinations() {
        // Earlier scans indexed these and the wikilink export used to emit
        // the first form; cutting at the space aimed them at `decisions/my.md`.
        assert_eq!(
            linked("[d](decisions/my decision.md) [e](notes/a b/c d.md)"),
            ["decisions/my decision.md", "notes/a b/c d.md"]
        );
        assert_eq!(
            linked("[f](notes/my file (1).md)"),
            ["notes/my file (1).md"]
        );
        assert_eq!(
            linked("[g](notes/my page.md \"a title\")"),
            ["notes/my page.md"]
        );
        assert_eq!(linked("[h](b.md )"), ["b.md"]);
    }

    #[test]
    fn extract_links_steps_over_titles_holding_parens_and_links() {
        // A `)` or a link inside a title is not the end of the link, nor a link.
        assert_eq!(linked("[a](<b.md> \"t (c) [d](e.md)\")"), ["b.md"]);
        assert_eq!(linked("[a](c.md \"x :) [d](e.md)\")"), ["c.md"]);
        assert_eq!(linked("[a](f.md \"x (y\")"), ["f.md"]);
        assert_eq!(
            linked("[a](g.md 'single') [b](h.md (paren title))"),
            ["g.md", "h.md"]
        );
    }

    #[test]
    fn extract_links_handles_nested_and_escaped_labels() {
        for label in [
            "array[0] access",
            r"array\[0\] access",
            r"closing \] bracket",
            r"literal \\ [index]",
            "日本語 [索引]",
            "outer [middle [inner]] label",
        ] {
            assert_eq!(linked(&format!("[{label}](b.md)")), ["b.md"], "{label}");
        }
        assert_eq!(linked("[unclosed [inner](b.md)"), ["b.md"]);
        assert_eq!(linked("[outer [inner](b.md)](c.md)"), ["b.md"]);
        assert!(linked(r"\[literal](b.md)").is_empty());
        assert!(linked("![array[0] access](b.md)").is_empty());
        assert_eq!(linked("[outer ![icon](image.png)](b.md)"), ["b.md"]);
        assert_eq!(linked(r"\![label](b.md)"), ["b.md"]);
        assert!(linked("`[array[0] access](b.md)`").is_empty());
        assert!(linked("[unclosed [label]").is_empty());
    }

    #[test]
    fn export_labels_keep_their_link_when_reindexed() {
        let path = PagePath::new("concepts/a.md").unwrap();
        for label in ["array[0] access", r"C:\work\notes", "日本語 [索引]"] {
            let source = format!("[[notes/array.md|{label}]]");
            let rewritten = rewrite_local_wikilinks(&source, &path, "proj");
            assert_eq!(
                extract_links(&rewritten, &path)
                    .iter()
                    .map(|link| link.path.as_str())
                    .collect::<Vec<_>>(),
                ["notes/array.md"],
                "{rewritten}"
            );
        }
    }

    #[test]
    fn extract_links_stays_linear_on_nested_and_unclosed_labels() {
        let root = PagePath::new("here.md").unwrap();
        let started = std::time::Instant::now();
        for body in [
            "[".repeat(256 * 1024),
            format!("{}[kept](b.md)", "[".repeat(256 * 1024)),
            format!("{}{}(b.md)", "[".repeat(128 * 1024), "]".repeat(128 * 1024)),
        ] {
            let _ = extract_links(&body, &root);
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn extract_links_rejects_pointy_destination_followed_by_junk() {
        assert!(linked("[a](<b.md>zzz)").is_empty());
        // A title must be separated from the destination by whitespace.
        assert!(linked("[a](<b.md>\"t\")").is_empty());
        assert_eq!(linked("[a](<b.md> \"t\")"), ["b.md"]);
    }

    #[test]
    fn extract_links_stays_linear_on_unclosed_destinations() {
        // Each `](` used to rescan the rest of the line for a `)` that never
        // comes: 256 KiB of `[a](` took seconds in release, minutes in debug,
        // on the page-write path. The bound keeps it to a fraction of a second.
        let root = PagePath::new("here.md").unwrap();
        let started = std::time::Instant::now();
        for unit in ["[a](", "[a](<x>"] {
            let line = unit.repeat(256 * 1024 / unit.len());
            assert!(extract_links(&line, &root).is_empty());
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "destination scan is no longer linear: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn extract_links_bounds_destination_length_without_splitting_characters() {
        // Straddle the scan window with 1- and 2-byte characters: cutting
        // inside a character must not panic.
        for n in 250..262 {
            let _ = linked(&format!("[a]({}.md)", "é".repeat(n)));
            let _ = linked(&format!("[a](a{}.md)", "é".repeat(n)));
        }
        assert!(linked(&format!("[a]({}.md)", "x".repeat(600))).is_empty());
        assert_eq!(linked(&format!("[a]({}.md)", "x".repeat(200))).len(), 1);
    }

    #[test]
    fn backtick_fence_info_string_cannot_contain_backticks() {
        // Not a fence: a paragraph opening with an inline code span.
        let md =
            "```code``` intro [[a/b.md]]\n[[c/d.md]]\n```\n[[e/f.md]]\n```\nafter [[g/h.md]]\n";
        assert_eq!(linked(md), ["a/b.md", "c/d.md", "g/h.md"]);

        let path = PagePath::new("concepts/a.md").unwrap();
        let rewritten = rewrite_local_wikilinks(md, &path, "proj");
        assert!(rewritten.contains("[a/b.md](../a/b.md)"), "{rewritten}");
        assert!(rewritten.contains("[[e/f.md]]"), "{rewritten}");

        // A tilde fence may carry backticks in its info string.
        assert!(linked("~~~ a`b\n[[x.md]]\n~~~\n").is_empty());
    }

    #[test]
    fn rewrite_local_wikilinks_wraps_each_destination_a_bare_link_cannot_hold() {
        let path = PagePath::new("concepts/a.md").unwrap();
        for target in [
            "notes/foo_(1).md",
            "notes/a).md",
            "notes/a(b.md",
            "notes/a\tb.md",
            "notes/a b).md",
            "notes/(x) y.md",
        ] {
            let rewritten = rewrite_local_wikilinks(&format!("See [[{target}]]."), &path, "proj");
            assert_eq!(
                extract_links(&rewritten, &path)
                    .iter()
                    .map(|l| l.path.as_str())
                    .collect::<Vec<_>>(),
                [target],
                "{rewritten:?} must index back to the page it was written from"
            );
        }
        // Balanced parentheses need no brackets; unbalanced ones do.
        let balanced = rewrite_local_wikilinks("[[notes/foo_(1).md]]", &path, "proj");
        assert!(balanced.ends_with("](../notes/foo_(1).md)"), "{balanced}");
        let unbalanced = rewrite_local_wikilinks("[[notes/a).md]]", &path, "proj");
        assert!(unbalanced.ends_with("](<../notes/a).md>)"), "{unbalanced}");
    }

    #[test]
    fn rewrite_local_wikilinks_keeps_targets_it_cannot_express_as_a_link() {
        // `<` or `>` in a destination that needs the brackets would need a
        // backslash escape the extractor does not read back.
        let path = PagePath::new("concepts/a.md").unwrap();
        let body = "See [[notes/a>b c.md]].";
        assert_eq!(rewrite_local_wikilinks(body, &path, "proj"), body);
        // Without whitespace or parens a bare destination holds them fine.
        let bare = rewrite_local_wikilinks("[[notes/a>b.md]]", &path, "proj");
        assert!(bare.ends_with("](../notes/a>b.md)"), "{bare}");
    }

    #[test]
    fn markdown_url_paths_decode_once_before_link_resolution() {
        for (destination, expected) in [
            ("notes/my%20note.md", "notes/my note.md"),
            ("notes/%E6%97%A5%E6%9C%AC.md", "notes/日本.md"),
            ("notes/my%23note.md#section", "notes/my#note.md"),
            ("notes/my%3Fnote.md?view=1", "notes/my?note.md"),
            ("notes/my%2520note.md", "notes/my%20note.md"),
            ("notes%2Fmy%20note.md", "notes/my note.md"),
            ("notes/%zz.md", "notes/%zz.md"),
            ("notes/%2.md", "notes/%2.md"),
        ] {
            let root = PagePath::new("source.md").unwrap();
            let links = extract_links(&format!("[note](<{destination}>)"), &root);
            assert_eq!(links.len(), 1, "{destination}");
            assert_eq!(links[0].path.as_str(), expected, "{destination}");
            assert_eq!(links[0].workspace, None);
            assert_eq!(links[0].project, None);
        }
        assert_eq!(linked("[[notes/my%20note.md]]"), ["notes/my%20note.md"]);
    }

    #[test]
    fn decoded_markdown_paths_cannot_escape_or_change_scope() {
        let root = PagePath::new("source.md").unwrap();
        for destination in [
            "%2e%2e/secret.md",
            "notes/%2e%2e/%2e%2e/secret.md",
            "notes%5Csecret.md",
            "%68ttps%3A%2F%2Fexample.com/secret.md",
            "%66ile%3Asecret.md",
            "notes/%FF.md",
            "notes/%00.md",
        ] {
            assert!(
                extract_links(&format!("[note](<{destination}>)"), &root).is_empty(),
                "{destination} must not become a page target"
            );
        }
        let links = extract_links("[note](other%3Anotes/secret.md)", &root);
        assert!(
            links
                .iter()
                .all(|link| link.workspace.is_none() && link.project.is_none())
        );
        let links = extract_links("[note](notes/%2e%2e/inside.md)", &root);
        assert_eq!(links[0].path.as_str(), "inside.md");
    }
}
