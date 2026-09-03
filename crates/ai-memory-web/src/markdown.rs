//! Markdown → HTML rendering for the read-only web browser, plus the
//! `[[wikilink]]` preprocessor that turns wiki shorthand into ordinary
//! markdown links before parsing.
//!
//! The renderer treats wiki content as untrusted (it can originate from
//! prompts, hooks, or LLM output), so raw HTML is escaped and unsafe
//! link schemes are neutralised.

use ai_memory_core::PagePath;
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};

/// Render a markdown body to HTML using GFM-ish defaults.
///
/// Raw HTML is escaped and unsafe link/image schemes are neutralised.
/// Wiki content can be derived from prompts, hooks, or LLM output, so
/// the browser surface must treat it as untrusted.
///
/// `[[wiki links]]` (with the `[[target|label]]`, `[[project:path]]`, and
/// `[[workspace/project:path]]` variants the engine's link extractor
/// understands) are rendered as clickable internal links resolved against
/// the page's own `workspace`/`project` unless the target carries its own
/// scope. Wikilinks inside fenced code blocks and inline code are left as
/// literal text.
#[must_use]
pub fn render(body: &str, workspace: &str, project: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_SMART_PUNCTUATION);

    // Rewrite `[[wikilinks]]` into ordinary markdown links BEFORE parsing.
    // pulldown-cmark consumes `[...]` as reference-link syntax, so the brackets
    // never survive as a single text node — preprocessing the source is the
    // robust hook. Code (fenced + inline) is skipped so `[[…]]` stays literal
    // there, mirroring the engine's link extractor.
    let body = preprocess_wikilinks(body, workspace, project);

    let parser =
        Parser::new_ext(&body, opts).map(|event| sanitize_event(event, workspace, project));
    let mut out = String::with_capacity(body.len() + body.len() / 4);
    html::push_html(&mut out, parser);
    out
}

/// Rewrite a relative in-wiki link target (a page like `concepts/foo.md`
/// or a namespace directory like `_lint/`) to a project-scoped URL, so it
/// resolves under the web mount's `<base href>` instead of against it. An
/// external URL, anchor, absolute path, or anything with a scheme is left
/// for [`safe_url`] to handle. Targets are treated as project-root
/// relative — the same convention `[[wikilinks]]` use — which is what the
/// OKF bundle-index page and generated cross-references rely on (#603).
fn scope_relative_link<'a>(dest: CowStr<'a>, workspace: &str, project: &str) -> CowStr<'a> {
    let trimmed = dest.trim();
    // Leave anchors, absolute paths, network paths, and any scheme
    // (http:, mailto:, javascript:, …) to safe_url.
    if trimmed.is_empty()
        || trimmed.starts_with('#')
        || trimmed.starts_with('/')
        || trimmed.starts_with('\\')
        || trimmed.contains(':')
    {
        return dest;
    }
    // Already project-scoped (a `[[wikilink]]` the preprocessor rewrote to
    // `w/<ws>/<proj>/p/…`, including cross-project/workspace targets):
    // don't scope it again or the path doubles.
    if trimmed.starts_with("w/") && trimmed.contains("/p/") {
        return dest;
    }
    // Split off a trailing #anchor/?query so it survives the rewrite.
    let (path_part, suffix) = match trimmed.find(['#', '?']) {
        Some(i) => (&trimmed[..i], &trimmed[i..]),
        None => (trimmed, ""),
    };
    if path_part.is_empty() || path_part.contains("..") {
        return dest; // don't rewrite traversal; safe_url/router will reject
    }
    let path_part = path_part.trim_end_matches('/');
    if path_part.is_empty() {
        return dest;
    }
    CowStr::Boxed(
        format!(
            "{}{suffix}",
            crate::templates::page_href(workspace, project, path_part)
        )
        .into_boxed_str(),
    )
}

/// Convert `[[target]]` / `[[target|label]]` spans into `[label](href)`
/// markdown links, skipping fenced code blocks, inline-code spans, and
/// 4-space-indented code blocks. Targets that aren't internal pages
/// (external schemes, traversal, empty) are left as literal `[[…]]`.
fn preprocess_wikilinks(body: &str, workspace: &str, project: &str) -> String {
    let mut out = String::with_capacity(body.len() + 64);
    // None when outside a fence; Some(char) carrying the opener glyph
    // (`'`' ` for ```` ``` ````, `'~'` for `~~~`) when inside one. The
    // CommonMark rule is that a fence closes only with the *same* glyph,
    // so opening with `~~~` ignores a `` ``` `` line in between and
    // vice versa.
    let mut fence: Option<char> = None;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let leading_indent = line.len() - trimmed.len();
        if let Some(kind) = fence_glyph(trimmed) {
            match fence {
                None => fence = Some(kind),
                Some(open) if open == kind => fence = None,
                _ => {} // Mismatched glyph inside a fenced block — literal text.
            }
            out.push_str(line);
            continue;
        }
        if fence.is_some() {
            out.push_str(line);
            continue;
        }
        // 4-space-indented (or tab-indented) lines are CommonMark code
        // blocks. A wikilink inside one must stay literal so it ends up
        // inside the rendered `<pre><code>…</code></pre>`. Blank-only
        // indented lines are pass-through (paragraph continuation).
        if !trimmed.is_empty() && (leading_indent >= 4 || line.starts_with('\t')) {
            out.push_str(line);
            continue;
        }
        // Split on backticks: even segments are outside inline code, odd ones
        // are inside it (left verbatim). Unbalanced backticks degrade safely.
        for (i, seg) in line.split('`').enumerate() {
            if i > 0 {
                out.push('`');
            }
            if i % 2 == 0 {
                rewrite_wikilinks_in_text(seg, workspace, project, &mut out);
            } else {
                out.push_str(seg);
            }
        }
    }
    out
}

/// If `trimmed` opens or closes a CommonMark code fence, return its
/// opener glyph (`` ` `` or `~`). CommonMark requires at least three
/// of the same glyph; we accept the lenient "starts with three" rule
/// to mirror the pulldown-cmark parser's behaviour for our preprocessor.
fn fence_glyph(trimmed: &str) -> Option<char> {
    if trimmed.starts_with("```") {
        Some('`')
    } else if trimmed.starts_with("~~~") {
        Some('~')
    } else {
        None
    }
}

/// Rewrite every `[[…]]` in a non-code text run into a markdown link.
fn rewrite_wikilinks_in_text(seg: &str, workspace: &str, project: &str, out: &mut String) {
    let mut rest = seg;
    loop {
        let Some(open) = rest.find("[[") else {
            out.push_str(rest);
            break;
        };
        let after = &rest[open + 2..];
        let Some(close) = after.find("]]") else {
            out.push_str(rest); // unterminated → literal
            break;
        };
        out.push_str(&rest[..open]);
        let raw = &after[..close];
        match wikilink_href_label(raw, workspace, project) {
            Some((href, label)) => {
                out.push('[');
                out.push_str(&escape_link_label(&label));
                out.push_str("](");
                out.push_str(&href);
                out.push(')');
            }
            None => {
                out.push_str("[[");
                out.push_str(raw);
                out.push_str("]]");
            }
        }
        rest = &after[close + 2..];
    }
}

/// Escape the characters that would prematurely close a markdown link label.
///
/// The wikilink rewriter emits `[<label>](<href>)`. Brackets and the
/// backslash need to escape, but so do parentheses — `]` plus an
/// unbalanced `(` inside the label lets the next `)` end the link
/// markup early. Without this, `[[a|x](y]]` rendered as
/// `[x](y](href)` and pulldown-cmark closed the link at the first `)`.
fn escape_link_label(label: &str) -> String {
    label
        .replace('\\', r"\\")
        .replace('[', r"\[")
        .replace(']', r"\]")
        .replace('(', r"\(")
        .replace(')', r"\)")
}

/// Resolve a `[[…]]` target (inner text, no brackets) into a relative page
/// href + display label. Returns `None` for empty/external/malformed targets
/// (callers then keep the literal `[[…]]`).
///
/// A `|` inside `raw` separates target from label at the FIRST pipe —
/// `[[a|b|c]]` → target `a`, label `b|c`. Targets are paths and must
/// not contain pipes; labels are display text and may.
fn wikilink_href_label(raw: &str, workspace: &str, project: &str) -> Option<(String, String)> {
    let (target, label) = match raw.split_once('|') {
        Some((t, l)) => (t.trim(), Some(l.trim())),
        None => (raw.trim(), None),
    };
    if target.is_empty() {
        return None;
    }
    // External / scheme-qualified targets are not internal wiki pages.
    // Bare fragment targets (`[[#anchor]]`) are page-internal anchors, not
    // wiki pages — also bail.
    if target.starts_with('#') || has_external_or_unsafe_scheme(target) {
        return None;
    }

    // Optional `[workspace/]project:` scope qualifier.
    let (ws, proj, path_part) = split_scope(target, workspace, project);

    // Strip anchor/query, reject non-page extensions, normalise the `.md`
    // suffix, then rely on the canonical page-path validator for traversal,
    // absolute paths, Windows prefixes, backslashes, and empty segments.
    let path = path_part.split(['#', '?']).next().unwrap_or("").trim();
    if path.is_empty() {
        return None;
    }
    let last_segment = path.rsplit('/').next().unwrap_or("");
    let path = if last_segment.contains('.') {
        if !path.ends_with(".md") {
            return None;
        }
        path.to_string()
    } else {
        format!("{path}.md")
    };
    let path = PagePath::new(path).ok()?;

    let href = crate::templates::page_href(ws, proj, path.as_str());
    let display = label
        .filter(|l| !l.is_empty())
        .unwrap_or(target)
        .to_string();
    Some((href, display))
}

/// Peel an optional `[workspace/]project:` scope off a wikilink target,
/// defaulting to the current page's `workspace`/`project`. A `scope` made of
/// anything other than `[-_/.alnum]` is treated as part of the path (so a
/// stray colon in a filename doesn't masquerade as a scope).
fn split_scope<'a>(
    target: &'a str,
    cur_ws: &'a str,
    cur_proj: &'a str,
) -> (&'a str, &'a str, &'a str) {
    if let Some((scope, rest)) = target.split_once(':') {
        let scope = scope.trim();
        let scope_ok = !scope.is_empty()
            && scope
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'));
        if scope_ok {
            return match scope.split_once('/') {
                Some((ws, proj)) if !proj.trim().is_empty() => {
                    (ws.trim(), proj.trim(), rest.trim())
                }
                Some(_) => (cur_ws, cur_proj, target), // malformed `ws/`
                None => (cur_ws, scope, rest.trim()),
            };
        }
    }
    (cur_ws, cur_proj, target)
}

fn sanitize_event<'a>(event: Event<'a>, workspace: &str, project: &str) -> Event<'a> {
    match event {
        Event::Html(s) | Event::InlineHtml(s) => Event::Text(s),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            // Scope relative in-wiki targets first (#603), then run the
            // scheme allowlist over whatever remains.
            dest_url: safe_url(scope_relative_link(dest_url, workspace, project)),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: safe_image_url(dest_url),
            title,
            id,
        }),
        other => other,
    }
}

fn safe_url(url: CowStr<'_>) -> CowStr<'_> {
    let trimmed = url.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with('/')
        || lower.starts_with('#')
        || !lower.contains(':')
    {
        url
    } else {
        CowStr::Boxed("#".into())
    }
}

/// Allow images only from the current origin.
///
/// Unlike links, images are fetched as soon as untrusted markdown is viewed.
/// An absolute or network-path URL would therefore disclose the viewer's IP,
/// user agent, and view timing without a click. Keep ordinary external links
/// clickable, but neutralise every non-relative image target. Backslashes are
/// rejected because browsers can interpret them as URL path separators.
fn safe_image_url(url: CowStr<'_>) -> CowStr<'_> {
    let trimmed = url.trim_start();
    let is_network_path = trimmed.starts_with("//")
        || trimmed.starts_with("\\\\")
        || trimmed.starts_with("/\\")
        || trimmed.starts_with("\\/");
    let is_relative =
        trimmed.starts_with('/') || trimmed.starts_with('#') || !trimmed.contains(':');

    if !trimmed.is_empty() && is_relative && !is_network_path && !trimmed.contains('\\') {
        url
    } else {
        CowStr::Boxed("#".into())
    }
}

/// True when `target` carries a URL scheme that should never be
/// resolved as a wiki page. Two paths feed this rule:
///   * the wikilink rewriter, which falls back to literal `[[…]]`;
///   * `safe_url`, which independently allows the four explicitly
///     listed schemes (http/https/mailto + root/fragment/no-colon
///     relatives) and rewrites everything else to `#`.
///
/// The two checks are NOT the same: `safe_url` ACCEPTS by allowlist;
/// this helper REJECTS by denylist. The shared concern is "this looks
/// like an external scheme" — keeping the list in one place lets a
/// future scheme (e.g. `vscode://`) extend both surfaces at once.
fn has_external_or_unsafe_scheme(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    target.contains("://")
        || lower.starts_with("mailto:")
        || lower.starts_with("data:")
        || lower.starts_with("javascript:")
        || lower.starts_with("vbscript:")
        || lower.starts_with("tel:")
        || lower.starts_with("file:")
}

/// Escape text for insertion into an HTML template while preserving the
/// fixed `<mark>` tags emitted by SQLite FTS snippets.
#[must_use]
pub fn escape_snippet(snippet: &str) -> String {
    escape_html(snippet)
        .replace("&lt;mark&gt;", "<mark>")
        .replace("&lt;/mark&gt;", "</mark>")
}

fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Drop the leading H1 from a markdown body if present. Static-site
/// convention: the first H1 IS the page title, and the page template
/// already renders the title in its header — leaving it in the body
/// duplicates it on screen. No-op when the body doesn't start with
/// an H1 (handles `# Title`, both ATX `# Title` and setext
/// `Title\n=====` forms).
#[must_use]
pub fn strip_leading_h1(body: &str) -> &str {
    // Skip any leading blank lines.
    let trimmed = body.trim_start_matches(['\n', '\r']);
    // ATX form: `# Title` (one `#`, NOT `## …`).
    if let Some(rest) = trimmed.strip_prefix("# ") {
        let after_line = rest.find('\n').map_or("", |nl| &rest[nl + 1..]);
        return after_line.trim_start_matches(['\n', '\r']);
    }
    // Setext form: `Title\n====…` (1+ equals signs). Look ahead.
    if let Some((first_line, after_first)) = trimmed.split_once('\n')
        && !first_line.is_empty()
        && let Some((second_line, after_second)) = after_first.split_once('\n')
        && !second_line.is_empty()
        && second_line.chars().all(|c| c == '=')
    {
        return after_second.trim_start_matches(['\n', '\r']);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_basic_markdown() {
        let html = render("# Hello\n\nworld", "default", "scratch");
        assert!(html.contains("<h1>Hello</h1>"));
        assert!(html.contains("<p>world</p>"));
    }

    #[test]
    fn renders_tables() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |";
        let html = render(md, "default", "scratch");
        assert!(html.contains("<table>"));
        assert!(html.contains("<td>1</td>"));
    }

    #[test]
    fn escapes_raw_html() {
        let html = render("<script>alert(1)</script>", "default", "scratch");
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn neutralises_javascript_links() {
        let html = render("[x](javascript:alert(1))", "default", "scratch");
        assert!(html.contains("href=\"#\""));
        assert!(!html.contains("javascript:"));
    }

    #[test]
    fn preserves_clickable_external_links() {
        let html = render("[docs](https://example.com/docs)", "default", "scratch");
        assert!(html.contains(r#"href="https://example.com/docs""#));
    }

    #[test]
    fn scopes_relative_links_to_the_project() {
        // #603: the OKF bundle index's directory links and any relative
        // in-wiki link must resolve under the web mount, not against it.
        let html = render(
            "[lint](_lint/) and [foo](concepts/foo.md)",
            "default",
            "scratch",
        );
        assert!(
            html.contains(r#"href="w/default/scratch/p/_lint""#),
            "directory link must be project-scoped: {html}"
        );
        assert!(
            html.contains(r#"href="w/default/scratch/p/concepts/foo.md""#),
            "relative page link must be project-scoped: {html}"
        );
        // External, anchor, and dangerous schemes are untouched by the scope pass.
        let ext = render(
            "[x](https://e.com) [a](#h) [j](javascript:alert(1))",
            "d",
            "p",
        );
        assert!(ext.contains(r#"href="https://e.com""#));
        assert!(ext.contains(r##"href="#h""##));
        assert!(!ext.contains("javascript:"));
    }

    #[test]
    fn neutralises_external_images_without_hiding_alt_text() {
        for target in [
            "https://example.invalid/pixel.png",
            "http://example.invalid/pixel.png",
            "//example.invalid/pixel.png",
            r"\\example.invalid\pixel.png",
            r"/\example.invalid/pixel.png",
            "data:image/png;base64,AA==",
        ] {
            let markdown = format!("![remote pixel]({target})");
            let html = render(&markdown, "default", "scratch");
            assert!(html.contains("src=\"#\""), "target {target}: {html}");
            assert!(
                html.contains("alt=\"remote pixel\""),
                "target {target}: {html}"
            );
            assert!(!html.contains("example.invalid"), "target {target}: {html}");
        }
    }

    #[test]
    fn preserves_relative_images() {
        for target in [
            "images/diagram.png",
            "../shared/diagram.png",
            "/static/logo.svg",
        ] {
            let markdown = format!("![local]({target})");
            let html = render(&markdown, "default", "scratch");
            assert!(
                html.contains(&format!(r#"src="{target}""#)),
                "target {target}: {html}"
            );
        }
    }

    #[test]
    fn escapes_search_snippet_but_keeps_marks() {
        let out = escape_snippet("<script>x</script> <mark>hit</mark>");
        assert!(out.contains("&lt;script&gt;x&lt;/script&gt;"));
        assert!(out.contains("<mark>hit</mark>"));
    }

    #[test]
    fn strip_atx_h1_drops_first_heading() {
        let out = strip_leading_h1("# Title\n\nbody text\n");
        assert_eq!(out, "body text\n");
    }

    #[test]
    fn strip_atx_h1_tolerates_leading_blank_lines() {
        let out = strip_leading_h1("\n\n# Title\n\nbody\n");
        assert_eq!(out, "body\n");
    }

    #[test]
    fn strip_atx_h1_leaves_h2_alone() {
        let out = strip_leading_h1("## Subhead\n\nbody\n");
        assert_eq!(out, "## Subhead\n\nbody\n");
    }

    #[test]
    fn strip_atx_h1_leaves_body_without_title_alone() {
        let out = strip_leading_h1("just a paragraph\n");
        assert_eq!(out, "just a paragraph\n");
    }

    #[test]
    fn strip_setext_h1_drops_first_heading() {
        let out = strip_leading_h1("Title\n=====\n\nbody\n");
        assert_eq!(out, "body\n");
    }

    #[test]
    fn strip_does_not_eat_setext_h2() {
        // `----` underlines are H2, not H1. Leave them alone.
        let out = strip_leading_h1("Title\n----\n\nbody\n");
        assert_eq!(out, "Title\n----\n\nbody\n");
    }

    #[test]
    fn wikilink_resolves_against_current_project() {
        let html = render("see [[notes/foo]] here", "default", "scratch");
        assert!(
            html.contains(r#"<a href="w/default/scratch/p/notes/foo.md">notes/foo</a>"#),
            "got: {html}"
        );
    }

    #[test]
    fn wikilink_label_and_md_suffix() {
        // explicit label wins; `.md` appended when the last segment is bare.
        let html = render("[[notes/foo|the foo]] and [[bar.md]]", "default", "scratch");
        assert!(html.contains(r#">the foo</a>"#), "label: {html}");
        assert!(
            html.contains(r#"href="w/default/scratch/p/notes/foo.md""#),
            "suffix add: {html}"
        );
        assert!(
            html.contains(r#"href="w/default/scratch/p/bar.md""#),
            "suffix keep: {html}"
        );
    }

    #[test]
    fn wikilink_cross_project_and_cross_workspace_scope() {
        let html = render(
            "[[otherproj:notes/x]] [[ws2/proj2:y]]",
            "default",
            "scratch",
        );
        assert!(
            html.contains(r#"href="w/default/otherproj/p/notes/x.md""#),
            "project scope: {html}"
        );
        assert!(
            html.contains(r#"href="w/ws2/proj2/p/y.md""#),
            "workspace/project scope: {html}"
        );
    }

    #[test]
    fn wikilink_not_linkified_in_code() {
        let fenced = render("```\n[[notes/foo]]\n```", "default", "scratch");
        assert!(!fenced.contains("<a href"), "fenced: {fenced}");
        assert!(fenced.contains("[[notes/foo]]"), "fenced literal: {fenced}");

        let inline = render("use `[[notes/foo]]` literally", "default", "scratch");
        assert!(!inline.contains("<a href"), "inline code: {inline}");
    }

    #[test]
    fn malformed_or_external_wikilink_kept_literal() {
        // unterminated
        let a = render("text [[notes/foo without close", "default", "scratch");
        assert!(a.contains("[[notes/foo without close"), "unterminated: {a}");
        // external scheme inside [[ ]] is not an internal page → literal
        let b = render("[[https://example.com]]", "default", "scratch");
        assert!(!b.contains("<a href"), "external: {b}");
        // path traversal rejected
        let c = render("[[../etc/passwd]]", "default", "scratch");
        assert!(!c.contains("<a href"), "traversal: {c}");
        // non-page extensions and invalid page paths are rejected
        let d = render(
            "[[notes/foo.txt]] [[/absolute]] [[notes/./foo]]",
            "default",
            "scratch",
        );
        assert!(!d.contains("<a href"), "invalid page path: {d}");
    }

    /// A `~~~` fence must NOT be closed by a `` ``` `` line and vice
    /// versa (audit finding: same-glyph close rule per CommonMark).
    /// Without the fix, opening with `~~~` and including a `` ``` ``
    /// line ended the block early and linkified wikilinks afterwards.
    #[test]
    fn wikilink_respects_fence_glyph() {
        let md = "~~~\n[[a/b]]\n```\n[[c/d]]\n~~~\nafter [[e/f]]\n";
        let html = render(md, "default", "scratch");
        // Inside the tilde-fenced block, BOTH wikilinks must stay literal.
        // The backtick line is NOT a fence here.
        assert!(html.contains("[[a/b]]"), "a/b literal in fence: {html}");
        assert!(html.contains("[[c/d]]"), "c/d literal in fence: {html}");
        // After the tilde fence closes, the wikilink IS linkified.
        assert!(
            html.contains(r#"href="w/default/scratch/p/e/f.md""#),
            "post-fence link: {html}"
        );
    }

    /// 4-space-indented code blocks (CommonMark) must NOT be rewritten —
    /// the wikilink should survive as literal text inside `<pre><code>`.
    #[test]
    fn wikilink_inside_indented_code_block_stays_literal() {
        let md = "Paragraph.\n\n    [[notes/foo]]\n\nresume\n";
        let html = render(md, "default", "scratch");
        // The indented block renders as <pre><code>; the wikilink is
        // verbatim inside, with HTML-escaped square brackets.
        assert!(
            html.contains("<pre><code>") && html.contains("[[notes/foo]]"),
            "indented code wikilink: {html}"
        );
        assert!(
            !html.contains(r#"href="w/default/scratch/p/notes/foo.md""#),
            "must NOT linkify inside indented code: {html}"
        );
    }

    /// A label that contains `(` or `)` must not let the next `)` close
    /// the rewritten markdown link prematurely. Audit case: `[[a|x](y]]`.
    /// The escape must keep the parens inside the label.
    #[test]
    fn wikilink_label_with_parens_does_not_break_out() {
        let html = render("[[a|foo (bar)]]", "default", "scratch");
        // The href must point at the wiki page, not at any of the
        // parenthesised text.
        assert!(
            html.contains(r#"href="w/default/scratch/p/a.md""#),
            "href intact: {html}"
        );
        // The displayed label keeps the parentheses literally.
        assert!(html.contains("foo (bar)"), "paren label: {html}");
    }

    /// `[[<script>...]]`: malformed as a page path (HTML chars are
    /// not in `PagePath`'s allowed charset), so the wikilink stays
    /// literal; whatever HTML survives is escaped by `sanitize_event`.
    /// Regression guard for XSS via crafted target.
    #[test]
    fn wikilink_with_script_tag_target_does_not_emit_raw_html() {
        let html = render("[[<script>alert(1)</script>]]", "default", "scratch");
        assert!(!html.contains("<script>"), "raw html escaped: {html}");
        assert!(html.contains("&lt;script&gt;"), "html-escaped: {html}");
    }

    /// `[[#anchor]]` is an in-page anchor reference, not a wiki page.
    /// Must stay literal so the renderer doesn't fabricate a wiki link.
    #[test]
    fn wikilink_fragment_only_target_stays_literal() {
        let html = render("see [[#section]] above", "default", "scratch");
        assert!(html.contains("[[#section]]"), "fragment literal: {html}");
        assert!(
            !html.contains(r#"href="w/default/scratch/p/"#),
            "must not fabricate wiki link: {html}"
        );
    }

    /// Multiple pipes: split on the FIRST pipe, so `[[a|b|c]]` has
    /// target `a`, label `b|c`. Document the choice with an explicit
    /// assertion (a future "split on last pipe" change must update
    /// this test).
    #[test]
    fn wikilink_multiple_pipes_split_at_first() {
        let html = render("[[a|b|c]]", "default", "scratch");
        assert!(html.contains(r#"href="w/default/scratch/p/a.md""#));
        assert!(html.contains("b|c"), "label keeps later pipes: {html}");
    }

    /// External schemes inside `[[…]]` all stay literal — the wikilink
    /// rewriter and `safe_url` agree on what is "external", so the
    /// authoritative list is exercised end-to-end.
    #[test]
    fn wikilink_external_schemes_all_kept_literal() {
        for raw in [
            "[[http://x]]",
            "[[https://x]]",
            "[[mailto:x@y]]",
            "[[data:text/html,x]]",
            "[[javascript:alert(1)]]",
            "[[vbscript:msgbox]]",
            "[[tel:+1]]",
            "[[file:///etc/passwd]]",
        ] {
            let html = render(raw, "default", "scratch");
            assert!(
                !html.contains("<a href"),
                "{raw} must stay literal, got: {html}"
            );
        }
    }
}
