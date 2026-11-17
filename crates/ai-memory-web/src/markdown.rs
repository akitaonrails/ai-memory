//! Markdown → HTML rendering for the read-only web browser, plus the
//! `[[wikilink]]` preprocessor that turns wiki shorthand into ordinary
//! markdown links before parsing.
//!
//! The renderer treats wiki content as untrusted (it can originate from
//! prompts, hooks, or LLM output), so raw HTML is escaped and unsafe
//! link schemes are neutralised.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;

use ai_memory_core::{DEFAULT_WORKSPACE_NAME, GLOBAL_SCOPE_PROJECT, PagePath};
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

/// Render a markdown body to HTML using GFM-ish defaults.
///
/// Raw HTML is escaped and unsafe link/image schemes are neutralised.
/// Wiki content can be derived from prompts, hooks, or LLM output, so
/// the browser surface must treat it as untrusted.
///
/// `[[wiki links]]` (with the `[[target|label]]`, `[[project:path]]`,
/// `[[workspace/project:path]]`, and reserved `[[_global:path]]` variants
/// the engine's link extractor understands) are rendered as clickable
/// internal links resolved against the page's own `workspace`/`project`
/// unless the target carries its own scope. `[[_global:path]]` always
/// points at the `_global` project in the default workspace. Wikilinks
/// inside fenced code blocks and inline code are left as literal text.
#[must_use]
pub fn render_document(body: &str, workspace: &str, project: &str) -> Rendered {
    // Rewrite `[[wikilinks]]` into ordinary markdown links BEFORE parsing.
    // pulldown-cmark consumes `[...]` as reference-link syntax, so the brackets
    // never survive as a single text node — preprocessing the source is the
    // robust hook. Code (fenced + inline) is skipped so `[[…]]` stays literal
    // there, mirroring the engine's link extractor.
    let body = preprocess_wikilinks(body, workspace, project);

    let parser = Parser::new_ext(&body, options());
    let mut with_headings = HeadingIdAssigner::new(parser);
    let sanitized = with_headings
        .by_ref()
        .map(|event| sanitize_event(event, workspace, project));
    let mut out = String::with_capacity(body.len() + body.len() / 4);
    html::push_html(&mut out, sanitized);
    Rendered {
        html: out,
        outline: with_headings.outline,
    }
}

/// [`render_document`] without the outline.
#[cfg(test)]
fn render(body: &str, workspace: &str, project: &str) -> String {
    render_document(body, workspace, project).html
}

/// Render bundled product docs with repository-aware links, without wiki routing.
pub(crate) fn render_reference_document(
    body: &str,
    mut rewrite: impl FnMut(&str) -> String,
) -> Rendered {
    let mut headings = HeadingIdAssigner::new(Parser::new_ext(body, options()));
    let events = headings
        .by_ref()
        .map(|event| sanitize_event_with_links(event, |url| CowStr::Boxed(rewrite(&url).into())));
    let mut html = String::new();
    html::push_html(&mut html, events);
    Rendered {
        html,
        outline: headings.outline,
    }
}

/// A rendered page body and the headings it contains.
pub struct Rendered {
    /// The sanitized HTML.
    pub html: String,
    /// Every heading in document order, with the `id` the HTML carries.
    pub outline: Vec<OutlineEntry>,
}

/// One heading of a rendered page.
pub struct OutlineEntry {
    /// Heading level, 1 to 6.
    pub level: u8,
    /// Plain text of the heading.
    pub text: String,
    /// The `id` attribute rendered on the heading.
    pub id: String,
}

/// The GFM-ish parser options the page renders with. The wikilink
/// preprocessor parses with the same ones, so it skips exactly the code
/// the renderer will show as code.
fn options() -> Options {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_SMART_PUNCTUATION);
    opts
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
    let Some(decoded) = ai_memory_wiki::decode_markdown_destination_path(path_part) else {
        return CowStr::Borrowed("#");
    };
    let decoded_path = decoded.as_ref();
    if decoded_path.is_empty()
        || decoded_path.contains("..")
        || decoded_path.starts_with('/')
        || decoded_path.contains(['\\', ':'])
    {
        return if decoded_path == path_part {
            dest
        } else {
            CowStr::Borrowed("#")
        };
    }
    let mut path_part = decoded_path.trim_end_matches('/');
    while let Some(rest) = path_part.strip_prefix("./") {
        path_part = rest;
    }
    // `.//x` would leave a leading `/` and an empty first segment.
    if path_part.is_empty() || path_part == "." || path_part.starts_with('/') {
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
/// markdown links, skipping code (fenced and indented code blocks and
/// inline-code spans) and raw HTML blocks. Targets that aren't internal pages (external
/// schemes, traversal, empty) are left as literal `[[…]]`.
///
/// What counts as code is what the renderer's own parser reads as code,
/// not a guess from indentation: four spaces open a code block only where
/// CommonMark says they do, so a nested list item or a paragraph's
/// continuation line indented four spaces is text, and its wikilink is
/// rewritten like any other (the engine's link extractor indexes it).
///
/// An HTML block is shown as escaped source text, so markdown inside it
/// is never parsed: a rewritten wikilink would surface as its generated
/// `[label](w/…/p/….md)` markup instead of the `[[…]]` the page holds.
fn preprocess_wikilinks(body: &str, workspace: &str, project: &str) -> String {
    let mut out = String::with_capacity(body.len() + 64);
    let mut pos = 0;
    for code in literal_ranges(body) {
        rewrite_wikilinks_in_lines(&body[pos..code.start], workspace, project, &mut out);
        out.push_str(&body[code.clone()]);
        pos = code.end;
    }
    rewrite_wikilinks_in_lines(&body[pos..], workspace, project, &mut out);
    out
}

/// Byte ranges of `body` the renderer shows verbatim (fenced and indented
/// code blocks, inline-code spans, raw HTML blocks) as its parser reads
/// them, in document order and without overlap.
fn literal_ranges(body: &str) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for (event, range) in Parser::new_ext(body, options()).into_offset_iter() {
        if matches!(
            event,
            Event::Start(Tag::CodeBlock(_) | Tag::HtmlBlock) | Event::Code(_)
        ) && ranges.last().is_none_or(|last| range.start >= last.end)
        {
            ranges.push(range);
        }
    }
    ranges
}

/// Rewrite the wikilinks of a run of non-code text one line at a time, so
/// an unterminated `[[` never pairs with a `]]` on a later line.
fn rewrite_wikilinks_in_lines(text: &str, workspace: &str, project: &str, out: &mut String) {
    for line in text.split_inclusive('\n') {
        rewrite_wikilinks_in_text(line, workspace, project, out);
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

    // Split off a trailing #anchor/?query so it survives the rewrite,
    // matching scope_relative_link.
    let (path_part, suffix) = match path_part.find(['#', '?']) {
        Some(i) => (&path_part[..i], &path_part[i..]),
        None => (path_part, ""),
    };

    // Reject non-page extensions, normalise the `.md` suffix, then rely on
    // the canonical page-path validator for traversal, absolute paths,
    // Windows prefixes, backslashes, and empty segments.
    //
    // A directory target (trailing slash, empty last segment) and a
    // stem-less `.md` cannot name a page; leaving them as literal text
    // aligns web rendering with the wiki link extractor.
    let path = path_part.trim();
    if path.is_empty() {
        return None;
    }
    let last_segment = path.rsplit('/').next().unwrap_or("");
    if last_segment.is_empty() || last_segment == ".md" {
        return None;
    }
    let path = if last_segment.contains('.') {
        if !path.ends_with(".md") {
            return None;
        }
        path.to_string()
    } else {
        format!("{path}.md")
    };
    let path = PagePath::new(path).ok()?;

    let href = format!(
        "{}{}",
        crate::templates::page_href(ws, proj, path.as_str()),
        encode_link_suffix(suffix)
    );
    let display = label
        .filter(|l| !l.is_empty())
        .unwrap_or(target)
        .to_string();
    Some((href, display))
}

/// Percent-encode a wikilink's `#anchor` / `?query` suffix for a Markdown
/// link destination. A space, parenthesis, angle bracket, quote or backslash
/// would end or break the bare `[label](href)` destination; the URI delimiters
/// a fragment or query uses (and existing `%` escapes) stay as written.
fn encode_link_suffix(suffix: &str) -> String {
    let mut out = String::with_capacity(suffix.len());
    for byte in suffix.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
            | b':'
            | b'@'
            | b'/'
            | b'?'
            | b'#'
            | b'%' => out.push(byte as char),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(&mut out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Peel an optional `[workspace/]project:` scope off a wikilink target,
/// defaulting to the current page's `workspace`/`project`. A `scope` made of
/// anything other than `[-_/.alnum]` is treated as part of the path (so a
/// stray colon in a filename doesn't masquerade as a scope).
///
/// `[[_global:path]]` always names the reserved preferences project in
/// [`DEFAULT_WORKSPACE_NAME`], even when the current page lives elsewhere
/// (issue #1042). An explicit `[[workspace/_global:path]]` keeps the named
/// workspace.
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
                None if scope == GLOBAL_SCOPE_PROJECT => {
                    (DEFAULT_WORKSPACE_NAME, scope, rest.trim())
                }
                None => (cur_ws, scope, rest.trim()),
            };
        }
    }
    (cur_ws, cur_proj, target)
}

/// Iterator adapter that ensures every `Tag::Heading` event has a unique `id`.
///
/// If a heading does not already carry an `id`, its inner text events are
/// buffered until `TagEnd::Heading`, converted to a URL-safe slug, deduplicated
/// against other headings on the page, and assigned to the heading start tag.
struct HeadingIdAssigner<'a, I> {
    iter: I,
    slugger: HeadingSlugger,
    queue: VecDeque<Event<'a>>,
    outline: Vec<OutlineEntry>,
}

impl<'a, I: Iterator<Item = Event<'a>>> HeadingIdAssigner<'a, I> {
    fn new(iter: I) -> Self {
        Self {
            iter,
            slugger: HeadingSlugger::default(),
            queue: VecDeque::new(),
            outline: Vec::new(),
        }
    }
}

impl<'a, I: Iterator<Item = Event<'a>>> Iterator for HeadingIdAssigner<'a, I> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(event) = self.queue.pop_front() {
            return Some(event);
        }

        let event = self.iter.next()?;
        match event {
            Event::Start(Tag::Heading {
                level,
                id,
                classes,
                attrs,
            }) => {
                let mut text = String::new();
                let mut inner_events = Vec::new();
                let mut end_level = None;

                for next_event in self.iter.by_ref() {
                    match next_event {
                        Event::End(TagEnd::Heading(lvl)) => {
                            end_level = Some(lvl);
                            break;
                        }
                        Event::Text(s) => {
                            text.push_str(&s);
                            inner_events.push(Event::Text(s));
                        }
                        Event::Code(s) => {
                            text.push_str(&s);
                            inner_events.push(Event::Code(s));
                        }
                        Event::InlineMath(s) => {
                            text.push_str(&s);
                            inner_events.push(Event::InlineMath(s));
                        }
                        Event::DisplayMath(s) => {
                            text.push_str(&s);
                            inner_events.push(Event::DisplayMath(s));
                        }
                        other => {
                            inner_events.push(other);
                        }
                    }
                }

                let id_str = match id {
                    Some(custom) => self.slugger.deduplicate(custom.as_ref()),
                    None => self.slugger.slugify(&text),
                };
                self.outline.push(OutlineEntry {
                    level: level as u8,
                    text: text.trim().to_owned(),
                    id: id_str.clone(),
                });
                self.queue.push_back(Event::Start(Tag::Heading {
                    level,
                    id: Some(CowStr::Boxed(id_str.into_boxed_str())),
                    classes,
                    attrs,
                }));
                self.queue.extend(inner_events);
                if let Some(lvl) = end_level {
                    self.queue.push_back(Event::End(TagEnd::Heading(lvl)));
                }

                self.queue.pop_front()
            }
            other => Some(other),
        }
    }
}

/// State for tracking and deduplicating heading slugs across a rendered page.
#[derive(Default)]
struct HeadingSlugger {
    seen: HashSet<String>,
    counts: HashMap<String, usize>,
}

impl HeadingSlugger {
    fn slugify(&mut self, text: &str) -> String {
        let base = slugify(text);
        self.deduplicate(&base)
    }

    fn deduplicate(&mut self, base: &str) -> String {
        let base = if base.is_empty() { "section" } else { base };
        if self.seen.insert(base.to_string()) {
            return base.to_string();
        }
        let counter = self.counts.entry(base.to_string()).or_insert(1);
        loop {
            let candidate = format!("{base}-{counter}");
            *counter += 1;
            if self.seen.insert(candidate.clone()) {
                return candidate;
            }
        }
    }
}

/// Convert heading text to a URL-friendly slug.
///
/// Converts alphanumeric characters to lowercase, replaces whitespace,
/// dashes and hyphens with `-` (collapsing consecutive separators),
/// preserves underscores, and strips punctuation. Empty results fall back
/// to `"section"`.
fn slugify(text: &str) -> String {
    let mut slug = String::with_capacity(text.len());
    let mut prev_is_dash = false;

    for c in text.chars() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                slug.push(lc);
            }
            prev_is_dash = false;
        } else if c == '_' {
            slug.push(c);
            prev_is_dash = false;
        } else if (c == '-' || c == '—' || c == '–' || c.is_whitespace())
            && !slug.is_empty()
            && !prev_is_dash
        {
            slug.push('-');
            prev_is_dash = true;
        }
    }

    let trimmed = slug.trim_end_matches('-');
    if trimmed.is_empty() {
        "section".to_string()
    } else {
        trimmed.to_string()
    }
}

fn sanitize_event<'a>(event: Event<'a>, workspace: &str, project: &str) -> Event<'a> {
    sanitize_event_with_links(event, |url| scope_relative_link(url, workspace, project))
}

fn sanitize_event_with_links<'a>(
    event: Event<'a>,
    mut rewrite: impl FnMut(CowStr<'a>) -> CowStr<'a>,
) -> Event<'a> {
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
            dest_url: safe_url(rewrite(dest_url)),
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

/// Allow images only from relative paths under the web mount.
///
/// Unlike links, images are fetched as soon as untrusted markdown is viewed.
/// An absolute or network-path URL would therefore disclose the viewer's IP,
/// user agent, and view timing without a click. Keep ordinary external links
/// clickable, but neutralise every non-relative image target. Backslashes are
/// rejected because browsers can interpret them as URL path separators.
///
/// The fetch is also same-origin, so it carries the viewer's standing with
/// the server: a target that resolved to a machine or admin route would run
/// that route as the viewer. Root-relative targets and `..` segments (also
/// percent-encoded, which browsers resolve as dot segments) could climb out
/// of the web mount, and a first segment naming a server route reaches it
/// when the UI is mounted at the base-path root (`--web-slug /`).
fn safe_image_url(url: CowStr<'_>) -> CowStr<'_> {
    if is_safe_image_target(&url) {
        url
    } else {
        CowStr::Boxed("#".into())
    }
}

/// Top-level server routes outside the web UI. A relative image whose first
/// segment matches one would hit it when the UI is mounted at the root.
const SERVER_ROUTE_SEGMENTS: &[&str] = &[
    "admin",
    "auth",
    "handoff",
    "hook",
    "identity",
    "mcp",
    "workstream",
];

fn is_safe_image_target(url: &str) -> bool {
    // Browsers trim leading and trailing C0 controls and spaces; the HTML
    // writer percent-encodes the rest, so no other control reaches the URL.
    let cleaned = url.trim_matches(|c: char| c <= ' ');
    if cleaned.is_empty() {
        return false;
    }
    if cleaned.starts_with('#') {
        return true;
    }
    if cleaned.starts_with('/') || cleaned.contains(['\\', ':']) {
        return false;
    }
    let path = cleaned.split(['?', '#']).next().unwrap_or_default();
    let decoded = percent_decode_lossy(path);
    if decoded.starts_with('/')
        || decoded.contains('\\')
        || decoded.split('/').any(|segment| segment == "..")
    {
        return false;
    }
    let first = decoded
        .split('/')
        .find(|segment| !segment.is_empty() && *segment != ".")
        .unwrap_or_default();
    !SERVER_ROUTE_SEGMENTS
        .iter()
        .any(|route| first.eq_ignore_ascii_case(route))
}

/// Decode `%XX` escapes once, the way a URL parser sees dot segments.
fn percent_decode_lossy(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| char::from(b).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push(u8::try_from(hi * 16 + lo).unwrap_or(b'%'));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

/// Escape an internal FTS snippet for insertion into an HTML template.
///
/// SQLite wraps matches in control delimiters that the durable sanitizer
/// forbids in page content. Only those unforgeable delimiters become
/// `<mark>` tags; literal markup from a page is always escaped.
#[must_use]
pub fn escape_snippet(snippet: &str) -> String {
    ai_memory_store::highlighted_snippet_html(snippet)
}

/// Drop the leading H1 from a markdown body when it repeats `title`.
/// Static-site convention: the first H1 IS the page title, and the page
/// template already renders the title in its header — leaving it in the
/// body duplicates it on screen. An H1 that reads differently is not a
/// duplicate: a frontmatter `title:` outranks the H1, and a setext H1 never
/// names the page at all, so dropping one of those hid the only place the
/// page said it. No-op when the body doesn't start with that H1 (handles
/// both ATX `# Title` and setext `Title\n=====` forms).
#[must_use]
pub fn strip_leading_h1<'a>(body: &'a str, title: &str) -> &'a str {
    let title = title.trim();
    // Skip any leading blank lines.
    let trimmed = body.trim_start_matches(['\n', '\r']);
    // ATX form: `# Title` (one `#`, NOT `## …`).
    if let Some(rest) = trimmed.strip_prefix("# ") {
        let (heading, after_line) = rest.split_once('\n').unwrap_or((rest, ""));
        if heading.trim() == title {
            return after_line.trim_start_matches(['\n', '\r']);
        }
        return body;
    }
    // Setext form: `Title\n====…` (1+ equals signs). Look ahead.
    if let Some((first_line, after_first)) = trimmed.split_once('\n')
        && !first_line.is_empty()
        && first_line.trim() == title
    {
        let (second_line, after_second) = after_first.split_once('\n').unwrap_or((after_first, ""));
        let underline = second_line.trim_end_matches(['\r', ' ', '\t']);
        if !underline.is_empty() && underline.chars().all(|c| c == '=') {
            return after_second.trim_start_matches(['\n', '\r']);
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exported_wikilink_destinations_preserve_literal_character_references() {
        let page = PagePath::new("source.md").unwrap();
        for target in [
            "notes/a&copy;.md",
            "notes/a&amp;.md",
            "notes/a&b.md",
            "notes/a%26copy;&copy;.md",
        ] {
            let exported =
                ai_memory_wiki::rewrite_local_wikilinks(&format!("[[{target}]]"), &page, "proj");
            let destination = Parser::new_ext(&exported, options())
                .find_map(|event| match event {
                    Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url),
                    _ => None,
                })
                .expect("exported wikilink is an ordinary Markdown link");
            assert_eq!(
                ai_memory_wiki::decode_markdown_destination_path(&destination)
                    .expect("valid URL path"),
                target,
                "{exported} must target the literal filename after Markdown parsing"
            );
        }
    }

    #[test]
    fn renders_basic_markdown() {
        let html = render("# Hello\n\nworld", "default", "scratch");
        assert!(html.contains(r#"<h1 id="hello">Hello</h1>"#));
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
            "./images/diagram.png",
            "static/logo.png",
            "diagram.png?v=2",
            "#figure-1",
        ] {
            let markdown = format!("![local]({target})");
            let html = render(&markdown, "default", "scratch");
            assert!(
                html.contains(&format!(r#"src="{target}""#)),
                "target {target}: {html}"
            );
        }
    }

    /// A same-origin image fetch runs as the viewer, so a stored page must not
    /// be able to point one at a machine or admin route (the handoff claim, an
    /// admin job): root-relative, network-path, dot-segment (plain or
    /// percent-encoded) and root-mounted route targets are all neutralised.
    #[test]
    fn neutralises_images_that_reach_server_routes() {
        for target in [
            "/handoff?workspace=default&project=scratch",
            "/admin/backup",
            "/static/logo.png",
            "//evil.example/x.png",
            "../../../../handoff?workspace=default&project=scratch",
            "%2e%2e/%2e%2e/handoff?workspace=default&project=scratch",
            ".%2E/admin/backup",
            "images/%2e%2e/%2e%2e/handoff",
            "../shared/diagram.png",
            // `--web-slug /` puts the UI at the root, where these relative
            // targets resolve straight to the server routes.
            "handoff?workspace=default&project=scratch",
            "./handoff",
            ".//handoff",
            "Admin/backup",
            "mcp",
            "hook/batch",
            "workstream/runs",
            "identity",
            "%2fhandoff",
        ] {
            let markdown = format!("![pixel](<{target}>)");
            let html = render(&markdown, "default", "scratch");
            assert!(html.contains("src=\"#\""), "target {target}: {html}");
            assert!(html.contains("alt=\"pixel\""), "target {target}: {html}");
        }
    }

    #[test]
    fn escapes_search_snippet_but_keeps_marks() {
        let out = escape_snippet(&format!(
            "<script>x</script> {}hit{}",
            ai_memory_store::FTS_HIGHLIGHT_START,
            ai_memory_store::FTS_HIGHLIGHT_END
        ));
        assert!(out.contains("&lt;script&gt;x&lt;/script&gt;"));
        assert!(out.contains("<mark>hit</mark>"));
    }

    #[test]
    fn a_literal_mark_tag_in_page_text_stays_text() {
        assert_eq!(
            escape_snippet("use <mark> here <mark>hit</mark>"),
            "use &lt;mark&gt; here &lt;mark&gt;hit&lt;/mark&gt;"
        );
        assert_eq!(escape_snippet("see <mark> here"), "see &lt;mark&gt; here");
        assert_eq!(escape_snippet("a </mark> b"), "a &lt;/mark&gt; b");
        let highlighted = format!(
            "{}a{} x {}b{}",
            ai_memory_store::FTS_HIGHLIGHT_START,
            ai_memory_store::FTS_HIGHLIGHT_END,
            ai_memory_store::FTS_HIGHLIGHT_START,
            ai_memory_store::FTS_HIGHLIGHT_END
        );
        assert_eq!(
            escape_snippet(&highlighted),
            "<mark>a</mark> x <mark>b</mark>"
        );
        let escaped_inside = format!(
            "{}a&b{} <script>",
            ai_memory_store::FTS_HIGHLIGHT_START,
            ai_memory_store::FTS_HIGHLIGHT_END
        );
        assert_eq!(
            escape_snippet(&escaped_inside),
            "<mark>a&amp;b</mark> &lt;script&gt;"
        );
    }

    #[test]
    fn strip_atx_h1_drops_first_heading() {
        let out = strip_leading_h1("# Title\n\nbody text\n", "Title");
        assert_eq!(out, "body text\n");
    }

    #[test]
    fn strip_atx_h1_tolerates_leading_blank_lines() {
        let out = strip_leading_h1("\n\n# Title\n\nbody\n", "Title");
        assert_eq!(out, "body\n");
    }

    #[test]
    fn strip_atx_h1_leaves_h2_alone() {
        let out = strip_leading_h1("## Subhead\n\nbody\n", "Subhead");
        assert_eq!(out, "## Subhead\n\nbody\n");
    }

    #[test]
    fn strip_atx_h1_leaves_body_without_title_alone() {
        let out = strip_leading_h1("just a paragraph\n", "Title");
        assert_eq!(out, "just a paragraph\n");
    }

    #[test]
    fn strip_atx_h1_keeps_a_heading_that_is_not_the_title() {
        let body = "# Token refresh\n\nbody\n";
        assert_eq!(strip_leading_h1(body, "Auth decisions"), body);
        // A heading-only body that repeats the title leaves nothing.
        assert_eq!(strip_leading_h1("# Title", "Title"), "");
        assert_eq!(strip_leading_h1("# Title\r\n\r\nbody", "Title"), "body");
    }

    #[test]
    fn strip_setext_h1_drops_first_heading() {
        let out = strip_leading_h1("Title\n=====\n\nbody\n", "Title");
        assert_eq!(out, "body\n");
    }

    #[test]
    fn strip_setext_h1_keeps_a_heading_that_is_not_the_title() {
        let body = "Cache warmup\n============\n\nbody\n";
        assert_eq!(strip_leading_h1(body, "setext"), body);
    }

    #[test]
    fn strip_does_not_eat_setext_h2() {
        // `----` underlines are H2, not H1. Leave them alone.
        let out = strip_leading_h1("Title\n----\n\nbody\n", "Title");
        assert_eq!(out, "Title\n----\n\nbody\n");
    }

    #[test]
    fn strip_setext_h1_handles_crlf_and_trailing_whitespace() {
        let out = strip_leading_h1("Title\r\n=====\r\n\r\nbody\r\n", "Title");
        assert_eq!(out, "body\r\n");

        let out = strip_leading_h1("Title\n=====   \t\n\nbody\n", "Title");
        assert_eq!(out, "body\n");

        let out = strip_leading_h1("Title\r\n=====  \r\n\r\nbody\r\n", "Title");
        assert_eq!(out, "body\r\n");

        // Negative control: an underline with inner spaces is not a setext
        // H1 underline — the heading must survive stripping.
        let out = strip_leading_h1("Title\r\n== x ==\r\nbody", "Title");
        assert_eq!(out, "Title\r\n== x ==\r\nbody");
    }

    #[test]
    fn strip_setext_h1_handles_eof_without_trailing_newline() {
        assert_eq!(strip_leading_h1("Title\n=====", "Title"), "");
        assert_eq!(strip_leading_h1("Title\r\n=====\r", "Title"), "");
        assert_eq!(strip_leading_h1("\r\n\r\nTitle\r\n=====", "Title"), "");
    }

    #[test]
    fn markdown_url_destinations_are_decoded_once_before_route_encoding() {
        let html = render(
            "[space](notes/my%20note.md#part) [literal percent](notes/my%2520note.md) \
             [separator](notes%2Fmy%20note.md) [unicode](notes/%E6%97%A5%E6%9C%AC.md)",
            "default",
            "scratch",
        );
        for href in [
            "w/default/scratch/p/notes/my%20note.md#part",
            "w/default/scratch/p/notes/my%2520note.md",
            "w/default/scratch/p/notes/%E6%97%A5%E6%9C%AC.md",
        ] {
            assert!(html.contains(href), "{html}");
        }
        let wiki = render("[[notes/my%20note.md]]", "default", "scratch");
        assert!(
            wiki.contains("w/default/scratch/p/notes/my%2520note.md"),
            "{wiki}"
        );
    }

    #[test]
    fn decoded_markdown_destinations_cannot_change_the_web_scope() {
        for destination in [
            "%2e%2e/secret.md",
            "%2Fadmin/backup",
            "notes%5Csecret.md",
            "other%3Anotes/secret.md",
            "%68ttps%3A%2F%2Fexample.com/secret.md",
            "notes/%FF.md",
            "notes/%00.md",
        ] {
            let html = render(&format!("[note](<{destination}>)"), "default", "scratch");
            assert!(html.contains("href=\"#\""), "{destination}: {html}");
        }
        let html = render(
            "[note](%77/foreign/project/p/secret.md)",
            "default",
            "scratch",
        );
        assert!(
            html.contains("w/default/scratch/p/w/foreign/project/p/secret.md"),
            "{html}"
        );
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
    fn wikilink_preserves_anchor_and_query_suffix() {
        let html = render(
            "see [[notes/foo#section-1]] and [[notes/bar.md#section-2|Bar Section]]",
            "default",
            "scratch",
        );
        assert!(
            html.contains(
                r#"href="w/default/scratch/p/notes/foo.md#section-1">notes/foo#section-1</a>"#
            ),
            "bare anchor: {html}"
        );
        assert!(
            html.contains(r#"href="w/default/scratch/p/notes/bar.md#section-2">Bar Section</a>"#),
            "anchor with label: {html}"
        );

        let cross = render(
            "[[otherproj:notes/x#heading]] [[_global:python-env?v=1#setup|Setup]]",
            "default",
            "scratch",
        );
        assert!(
            cross.contains(
                r#"href="w/default/otherproj/p/notes/x.md#heading">otherproj:notes/x#heading</a>"#
            ),
            "cross-project anchor: {cross}"
        );
        assert!(
            cross.contains(r#"href="w/default/_global/p/python-env.md?v=1#setup">Setup</a>"#),
            "global scope query and anchor: {cross}"
        );
    }

    #[test]
    fn wikilink_suffix_with_space_or_parenthesis_stays_one_link() {
        let html = render(
            "[[notes/foo#my section]] and [[notes/foo#a)b]]",
            "default",
            "scratch",
        );
        assert!(
            html.contains(r#"href="w/default/scratch/p/notes/foo.md#my%20section">"#),
            "space in anchor: {html}"
        );
        assert!(
            html.contains(r#"href="w/default/scratch/p/notes/foo.md#a%29b">"#),
            "parenthesis in anchor: {html}"
        );
        assert!(
            !html.contains("b)"),
            "a ')' must not close the link early: {html}"
        );
        assert!(
            !html.contains("[["),
            "both wikilinks render as links: {html}"
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
    fn wikilink_global_scope_pins_to_default_workspace() {
        let html = render("see [[_global:python-env]] here", "other", "embodied-ai");
        assert!(
            html.contains(r#"href="w/default/_global/p/python-env.md""#),
            "project-only _global from another workspace: {html}"
        );

        let html = render("[[machine:notes/x]]", "other", "embodied-ai");
        assert!(
            html.contains(r#"href="w/other/machine/p/notes/x.md""#),
            "non-global project-only stays on the source workspace: {html}"
        );

        let html = render("[[other/_global:notes/x]]", "default", "scratch");
        assert!(
            html.contains(r#"href="w/other/_global/p/notes/x.md""#),
            "explicit workspace is kept: {html}"
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

    /// An HTML block renders as its escaped source, so a wikilink in it
    /// has to stay as written rather than turn into rewritten link markup.
    #[test]
    fn wikilink_kept_as_written_in_html_block() {
        let comment = render("<!-- see [[notes/foo]] -->", "default", "scratch");
        assert!(comment.contains("see [[notes/foo]]"), "comment: {comment}");
        assert!(!comment.contains("/p/notes/foo.md"), "comment: {comment}");

        let div = render("<div>\n[[notes/foo|Foo]]\n</div>", "default", "scratch");
        assert!(div.contains("[[notes/foo|Foo]]"), "div: {div}");

        // Inline HTML is only the tag itself; the text around it is still
        // a paragraph, and so is the paragraph after the block.
        let inline = render(
            "<div>x</div>\n\ntext <span>[[notes/foo]]</span>",
            "default",
            "scratch",
        );
        assert!(
            inline.contains(r#"<a href="w/default/scratch/p/notes/foo.md">notes/foo</a>"#),
            "inline: {inline}"
        );
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

    #[test]
    fn wikilink_directory_and_stemless_targets_stay_literal() {
        // A trailing slash (directory target) or a stem-less `.md` cannot
        // name a page; rewrite must not emit broken 404 links.
        let html = render(
            "[[notes/]] [[notes/.md]] [[.md]] [[other:notes/]] [[notes/|Folder]]",
            "default",
            "scratch",
        );
        assert!(!html.contains("<a href"), "no linkified targets: {html}");
        assert!(html.contains("[[notes/]]"));
        assert!(html.contains("[[notes/.md]]"));
        assert!(html.contains("[[.md]]"));
        assert!(html.contains("[[other:notes/]]"));
        assert!(html.contains("[[notes/|Folder]]"));
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

    /// Four spaces of indent make a code block only where CommonMark says
    /// so. A nested list item written with four spaces, and a paragraph's
    /// continuation line, are text: the engine's link extractor indexes the
    /// wikilink on them, so the page has to render it as a link too.
    #[test]
    fn wikilink_on_an_indented_line_that_is_not_code_is_linkified() {
        let nested = render(
            "- Decisions:\n    - see [[decisions/auth]]\n",
            "default",
            "scratch",
        );
        assert!(
            nested.contains(r#"href="w/default/scratch/p/decisions/auth.md""#),
            "nested list item: {nested}"
        );
        assert!(!nested.contains("<pre>"), "not a code block: {nested}");

        let continued = render(
            "The flow is described\n    in [[notes/flow]] and nowhere else.\n",
            "default",
            "scratch",
        );
        assert!(
            continued.contains(r#"href="w/default/scratch/p/notes/flow.md""#),
            "paragraph continuation: {continued}"
        );

        // Indented code inside a list item is still code, and stays literal.
        let code = render(
            "- step\n\n        run [[notes/foo]]\n",
            "default",
            "scratch",
        );
        assert!(code.contains("[[notes/foo]]"), "list code literal: {code}");
        assert!(!code.contains("<a href"), "list code not linkified: {code}");
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

    #[test]
    fn relative_link_with_leading_dot_slash_resolves() {
        let html = render(
            "[doc](./notes/foo.md) and [pointy](<./notes/bar.md>)",
            "default",
            "scratch",
        );
        assert!(
            html.contains(r#"href="w/default/scratch/p/notes/foo.md""#),
            "leading ./ must be stripped: {html}"
        );
        // The parser hands back a `<…>` destination without its brackets, so
        // only the `./` needs handling here.
        assert!(
            html.contains(r#"href="w/default/scratch/p/notes/bar.md""#),
            "pointy brackets and leading ./ must resolve: {html}"
        );
    }

    #[test]
    fn relative_link_with_leading_dot_slash_never_yields_an_empty_segment() {
        let html = render("[a](.//x.md) and [b](././y.md)", "default", "scratch");
        assert!(!html.contains("p//"), "empty path segment: {html}");
        assert!(
            html.contains(r#"href="w/default/scratch/p/y.md""#),
            "repeated ./ must be stripped: {html}"
        );
    }

    #[test]
    fn heading_ids_are_emitted_for_all_levels() {
        let md = "# Level 1\n\n## Level 2\n\n### Level 3\n\n#### Level 4\n\n##### Level 5\n\n###### Level 6";
        let html = render(md, "default", "scratch");
        assert!(html.contains(r#"<h1 id="level-1">Level 1</h1>"#), "{html}");
        assert!(html.contains(r#"<h2 id="level-2">Level 2</h2>"#), "{html}");
        assert!(html.contains(r#"<h3 id="level-3">Level 3</h3>"#), "{html}");
        assert!(html.contains(r#"<h4 id="level-4">Level 4</h4>"#), "{html}");
        assert!(html.contains(r#"<h5 id="level-5">Level 5</h5>"#), "{html}");
        assert!(html.contains(r#"<h6 id="level-6">Level 6</h6>"#), "{html}");
    }

    #[test]
    fn heading_ids_strip_punctuation_and_normalize_dashes() {
        let md = "## What \"deleted\" means: purge & destroy?\n\n### Scenario D — menu bar app\n\n#### 1. Introduction (v1.0)";
        let html = render(md, "default", "scratch");
        assert!(
            html.contains(r#"<h2 id="what-deleted-means-purge-destroy">"#),
            "punctuation and symbols: {html}"
        );
        assert!(
            html.contains(r#"<h3 id="scenario-d-menu-bar-app">"#),
            "em dash: {html}"
        );
        assert!(
            html.contains(r#"<h4 id="1-introduction-v10">"#),
            "numbered heading: {html}"
        );
    }

    #[test]
    fn heading_ids_with_formatting_and_code() {
        let md = "## Hello **bold** and `code` span";
        let html = render(md, "default", "scratch");
        assert!(
            html.contains(
                r#"<h2 id="hello-bold-and-code-span">Hello <strong>bold</strong> and <code>code</code> span</h2>"#
            ),
            "inline formatting: {html}"
        );
    }

    #[test]
    fn duplicate_headings_disambiguate_with_suffixes() {
        let md = "## Context\n\n## Context\n\n## Context 1\n\n## Context";
        let html = render(md, "default", "scratch");
        assert!(html.contains(r#"<h2 id="context">Context</h2>"#), "{html}");
        assert!(
            html.contains(r#"<h2 id="context-1">Context</h2>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<h2 id="context-1-1">Context 1</h2>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<h2 id="context-2">Context</h2>"#),
            "{html}"
        );
    }

    #[test]
    fn empty_or_punctuation_only_headings_fall_back_to_section() {
        let md = "## ???\n\n## !!!";
        let html = render(md, "default", "scratch");
        assert!(html.contains(r#"<h2 id="section">???</h2>"#), "{html}");
        assert!(html.contains(r#"<h2 id="section-1">!!!</h2>"#), "{html}");
    }

    #[test]
    fn heading_with_raw_html_and_quotes_escapes_markup_and_keeps_id_slug_safe() {
        let md = "## <img src=x onerror=alert(1)> \"q\"";
        let html = render(md, "default", "scratch");
        assert!(
            !html.contains("<img"),
            "raw heading HTML must not survive as markup: {html}"
        );
        // Smart punctuation renders the quotes as “q”; the HTML payload must
        // stay escaped text inside the heading.
        assert!(
            html.contains(r#"<h2 id="q">&lt;img src=x onerror=alert(1)&gt; “q”</h2>"#),
            "the heading must render escaped text with a slug id: {html}"
        );
        let id = html
            .split("id=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_else(|| panic!("no heading id emitted: {html}"));
        assert!(
            !id.is_empty()
                && id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'),
            "heading id must match ^[a-z0-9_-]+$, got: {id}"
        );
    }

    #[test]
    fn outline_ids_are_the_ids_rendered_on_the_headings() {
        let md = "## Intro\n\ntext\n\n### Intro\n\n## Intro\n\n## `Code` & more";
        let rendered = render_document(md, "default", "scratch");
        let ids: Vec<&str> = rendered.outline.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["intro", "intro-1", "intro-2", "code-more"]);
        let levels: Vec<u8> = rendered.outline.iter().map(|h| h.level).collect();
        assert_eq!(levels, [2, 3, 2, 2]);
        assert_eq!(rendered.outline[3].text, "Code & more");
        for id in ids {
            assert!(
                rendered.html.contains(&format!(r#" id="{id}">"#)),
                "id {id} missing: {}",
                rendered.html
            );
        }
    }

    #[test]
    fn wikilink_anchor_matches_generated_heading_id() {
        let md = "Jump to [[notes/foo#my-section|My Section]]\n\n## My Section";
        let html = render(md, "default", "scratch");
        assert!(
            html.contains(r#"href="w/default/scratch/p/notes/foo.md#my-section">My Section</a>"#),
            "link href: {html}"
        );
        assert!(
            html.contains(r#"<h2 id="my-section">My Section</h2>"#),
            "heading id: {html}"
        );
    }
}

#[cfg(test)]
mod reference_document_tests {
    #[test]
    fn bundled_guides_keep_unsafe_markup_inert_and_use_document_anchors() {
        let doc = crate::documentation::find("features").unwrap();
        let rendered = super::render_reference_document(
            "# Example\n\n<script>alert(1)</script>\n\n[x](javascript:alert) [guide](install.md) [local](#example)\n\n```text\n[[private-page]]\n```",
            |url| crate::documentation::link(doc, "docs/features", url),
        );
        assert!(!rendered.html.contains("<script>"));
        assert!(rendered.html.contains("&lt;script&gt;"));
        assert!(!rendered.html.contains("href=\"javascript:"));
        assert!(rendered.html.contains("href=\"docs/install\""));
        assert!(rendered.html.contains("href=\"docs/features#example\""));
        assert!(rendered.html.contains("[[private-page]]"));
        assert_eq!(rendered.outline[0].id, "example");
    }
}
