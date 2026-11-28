//! Local credential scrub shared by external capture clients.
//!
//! The server sanitizer (`ai-memory-core::sanitize`) re-scrubs everything
//! these clients send; this copy keeps credentials out of the local spool,
//! queue and preview files written before delivery. It follows the same
//! tiered architecture and rules for private keys, overlapping credentials,
//! invisible characters, and one-layer JSON string escapes: key blocks are
//! matched first, overlapping credential/bearer spans are merged before
//! rewriting so inner matches never leave outer fragments in clear, and
//! invisible formatting characters or JSON escapes do not hide a credential.

use regex::Regex;
use std::sync::OnceLock;

pub fn sanitize_external_text(input: &str) -> String {
    let mut offset = 0;
    let mut out = String::with_capacity(input.len());
    while let Some(ch) = input[offset..].chars().next() {
        let next = offset + ch.len_utf8();
        if ch == '\u{1f3f4}' {
            let tags = emoji_tag_run_len(&input[next..]);
            out.push_str(&input[offset..next + tags]);
            offset = next + tags;
            continue;
        }
        // Removed rather than replaced, before matching: a placeholder left
        // inside a credential would split it out of reach of every pattern.
        if ch == '\n'
            || ch == '\r'
            || ch == '\t'
            || !(ch.is_control()
                || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
                || is_hidden_text_char(offset, ch))
        {
            out.push(ch);
        }
        offset = next;
    }

    let mut shadows = Shadows::of(&out);

    // Tier 1 — Private keys: run first while base64 body lines are intact.
    for rule in private_key_patterns() {
        let spans = collect_rule_spans(&out, shadows.as_ref(), rule);
        if !spans.is_empty() {
            out = replace_spans(&out, spans, "[REDACTED PRIVATE KEY]");
            drop(shadows.take());
            shadows = Shadows::of(&out);
        }
    }

    // Tier 2 — Credential + Bearer group: collect spans for only the secret
    // bytes across all rules against the same snapshot, then merge overlapping
    // spans before rewriting once so an inner match never splits an outer
    // credential or bearer token.
    let mut tier2_spans = Vec::new();
    for rule in credential_patterns() {
        let spans = collect_rule_spans(&out, shadows.as_ref(), &rule.rule);
        tier2_spans.extend(
            spans
                .into_iter()
                .map(|(start, end)| (start, end, rule.is_credential)),
        );
    }
    if !tier2_spans.is_empty() {
        out = replace_credential_spans(&out, tier2_spans);
        drop(shadows.take());
        shadows = Shadows::of(&out);
    }

    // Tier 3 — Generic key/value rule: run after Tier 2 so already-redacted
    // markers are preserved and remaining opaque values are redacted.
    let kv_spans = collect_key_value_spans(&out, shadows.as_ref(), key_value_pattern());
    if !kv_spans.is_empty() {
        out = replace_spans(&out, kv_spans, "[REDACTED]");
    }

    out
}

/// Transport view of `text` with origin byte offsets so matches map back to
/// the original slice.
struct Shadow {
    text: String,
    origin: Vec<usize>,
}

/// Transport views of `text`:
/// - `fused` decodes one layer of JSON string escapes and drops
///   `is_default_ignorable` characters.
/// - `decoded_only` decodes JSON string escapes while preserving
///   `is_default_ignorable` characters as word/token boundaries; built only
///   when `text` both decoded an escape and dropped an ignorable character.
struct Shadows {
    fused: Shadow,
    decoded_only: Option<Shadow>,
}

impl Shadows {
    fn of(text: &str) -> Option<Self> {
        if !text.contains(|c: char| c == '\\' || is_default_ignorable(c)) {
            return None;
        }
        let mut fused_text = String::with_capacity(text.len());
        let mut fused_origin = Vec::with_capacity(text.len());
        let mut saw_escape = false;
        let mut dropped_ignorable = false;
        let mut index = 0;
        while let Some(c) = text[index..].chars().next() {
            let (c, len) = match json_escape_at(text.as_bytes(), index) {
                Some(decoded) => {
                    saw_escape = true;
                    decoded
                }
                None => (c, c.len_utf8()),
            };
            if is_default_ignorable(c) {
                dropped_ignorable = true;
            } else {
                fused_text.push(c);
                fused_origin.extend(std::iter::repeat_n(index, c.len_utf8()));
            }
            index += len;
        }
        let decoded_only = (saw_escape && dropped_ignorable).then(|| {
            let mut dec_text = String::with_capacity(text.len());
            let mut dec_origin = Vec::with_capacity(text.len());
            let mut index = 0;
            while let Some(c) = text[index..].chars().next() {
                let (c, len) = json_escape_at(text.as_bytes(), index).unwrap_or((c, c.len_utf8()));
                dec_text.push(c);
                dec_origin.extend(std::iter::repeat_n(index, c.len_utf8()));
                index += len;
            }
            Shadow {
                text: dec_text,
                origin: dec_origin,
            }
        });
        Some(Self {
            fused: Shadow {
                text: fused_text,
                origin: fused_origin,
            },
            decoded_only,
        })
    }
}

impl Shadow {
    fn original_span(&self, text: &str, start: usize, end: usize) -> (usize, usize) {
        let from = self.origin.get(start).copied().unwrap_or(text.len());
        let to = match end.checked_sub(1).and_then(|last| self.origin.get(last)) {
            Some(&last) if end > start => last + source_len_at(text, last),
            _ => from,
        };
        (from, to)
    }
}

fn source_len_at(text: &str, index: usize) -> usize {
    json_escape_at(text.as_bytes(), index).map_or_else(
        || text[index..].chars().next().map_or(0, char::len_utf8),
        |(_, len)| len,
    )
}

fn json_escape_at(bytes: &[u8], index: usize) -> Option<(char, usize)> {
    if bytes.get(index) != Some(&b'\\') {
        return None;
    }
    let decoded = match bytes.get(index + 1)? {
        b'"' => ('"', 2),
        b'\\' => ('\\', 2),
        b'/' => ('/', 2),
        b'b' => ('\u{8}', 2),
        b'f' => ('\u{c}', 2),
        b'n' => ('\n', 2),
        b'r' => ('\r', 2),
        b't' => ('\t', 2),
        b'u' => json_unicode_escape_at(bytes, index)?,
        _ => return None,
    };
    Some(decoded)
}

fn json_unicode_escape_at(bytes: &[u8], index: usize) -> Option<(char, usize)> {
    let unit = json_hex_unit(bytes, index + 2)?;
    if !(0xD800..=0xDBFF).contains(&unit) {
        return Some((char::from_u32(unit)?, 6));
    }
    if bytes.get(index + 6..index + 8)? != b"\\u" {
        return None;
    }
    let low = json_hex_unit(bytes, index + 8).filter(|low| (0xDC00..=0xDFFF).contains(low))?;
    let code = 0x1_0000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
    Some((char::from_u32(code)?, 12))
}

fn json_hex_unit(bytes: &[u8], at: usize) -> Option<u32> {
    bytes.get(at..at + 4)?.iter().try_fold(0, |unit, b| {
        Some((unit << 4) | char::from(*b).to_digit(16)?)
    })
}

/// Widen `span` so neither end falls between a backslash and the character it
/// escapes.
fn whole_escapes(text: &str, (mut start, mut end): (usize, usize)) -> (usize, usize) {
    let bytes = text.as_bytes();
    let odd_run_before = |at: usize| {
        bytes[..at]
            .iter()
            .rev()
            .take_while(|&&b| b == b'\\')
            .count()
            % 2
            == 1
    };
    if start > 0 && odd_run_before(start) {
        start -= 1;
    }
    if end > start && end < bytes.len() && odd_run_before(end) {
        end = (end - 1 + source_len_at(text, end - 1)).max(end);
    }
    (start, end)
}

fn spans_on_view(text: &str, shadow: Option<&Shadow>, rule: &SpanRule) -> Vec<(usize, usize)> {
    let view = shadow.map_or(text, |shadow| shadow.text.as_str());
    let map_span = |start: usize, end: usize| match shadow {
        Some(shadow) => shadow.original_span(text, start, end),
        None => (start, end),
    };
    if rule.secret_group {
        rule.regex
            .captures_iter(view)
            .filter_map(|caps| caps.name("secret").or_else(|| caps.get(0)))
            .map(|m| map_span(m.start(), m.end()))
            .collect()
    } else {
        rule.regex
            .find_iter(view)
            .map(|m| map_span(m.start(), m.end()))
            .collect()
    }
}

fn collect_rule_spans(
    text: &str,
    shadows: Option<&Shadows>,
    rule: &SpanRule,
) -> Vec<(usize, usize)> {
    let mut spans = spans_on_view(text, shadows.map(|s| &s.fused), rule);
    if let Some(shadows) = shadows {
        if let Some(decoded_only) = shadows.decoded_only.as_ref() {
            spans.extend(spans_on_view(text, Some(decoded_only), rule));
        }
        spans.extend(spans_on_view(text, None, rule));
    }
    spans
}

fn key_value_spans_on_view(
    text: &str,
    shadow: Option<&Shadow>,
    regex: &Regex,
) -> Vec<(usize, usize)> {
    let view = shadow.map_or(text, |shadow| shadow.text.as_str());
    let map_span = |start: usize, end: usize| match shadow {
        Some(shadow) => shadow.original_span(text, start, end),
        None => (start, end),
    };
    regex
        .captures_iter(view)
        .filter_map(|captures| {
            let value = captures.get(3)?;
            if value.as_str().chars().count() < 6
                || [
                    "[REDACTED]",
                    "[REDACTED CREDENTIAL]",
                    "[REDACTED PRIVATE KEY]",
                ]
                .contains(&value.as_str())
            {
                return None;
            }
            Some(map_span(value.start(), value.end()))
        })
        .collect()
}

fn collect_key_value_spans(
    text: &str,
    shadows: Option<&Shadows>,
    regex: &Regex,
) -> Vec<(usize, usize)> {
    let mut spans = key_value_spans_on_view(text, shadows.map(|s| &s.fused), regex);
    if let Some(shadows) = shadows {
        if let Some(decoded_only) = shadows.decoded_only.as_ref() {
            spans.extend(key_value_spans_on_view(text, Some(decoded_only), regex));
        }
        spans.extend(key_value_spans_on_view(text, None, regex));
    }
    spans
}

fn replace_spans(text: &str, spans: Vec<(usize, usize)>, marker: &str) -> String {
    let mut spans: Vec<_> = spans
        .into_iter()
        .map(|span| whole_escapes(text, span))
        .collect();
    spans.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut spans = spans.into_iter().peekable();
    while let Some((start, mut end)) = spans.next() {
        while let Some(&(next_start, next_end)) = spans.peek() {
            if next_start >= end {
                break;
            }
            end = end.max(next_end);
            spans.next();
        }
        out.push_str(&text[copied..start]);
        out.push_str(marker);
        copied = end;
    }
    out.push_str(&text[copied..]);
    out
}

fn replace_credential_spans(text: &str, spans: Vec<(usize, usize, bool)>) -> String {
    let mut spans: Vec<_> = spans
        .into_iter()
        .map(|(start, end, is_credential)| {
            let (start, end) = whole_escapes(text, (start, end));
            (start, end, is_credential)
        })
        .collect();
    spans.sort_unstable_by_key(|&(start, end, _)| (start, end));
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut spans = spans.into_iter().peekable();
    while let Some((start, mut end, mut is_credential)) = spans.next() {
        while let Some(&(next_start, next_end, next_is_credential)) = spans.peek() {
            if next_start >= end {
                break;
            }
            end = end.max(next_end);
            is_credential |= next_is_credential;
            spans.next();
        }
        out.push_str(&text[copied..start]);
        out.push_str(if is_credential {
            "[REDACTED CREDENTIAL]"
        } else {
            "[REDACTED]"
        });
        copied = end;
    }
    out.push_str(&text[copied..]);
    out
}

/// Tag characters outside an emoji flag sequence, the invisible math
/// operators, and a byte-order mark after the start of the text: characters
/// whose only realistic use in captured text is hiding it.
const fn is_hidden_text_char(offset: usize, ch: char) -> bool {
    matches!(ch, '\u{2061}'..='\u{2064}' | '\u{e0000}'..='\u{e007f}')
        || (ch == '\u{feff}' && offset > 0)
}

/// Unicode `Default_Ignorable_Code_Point` (UAX #44), the same set the server
/// sanitizer ignores while matching.
const fn is_default_ignorable(ch: char) -> bool {
    matches!(ch,
        '\u{ad}' | '\u{34f}' | '\u{61c}' | '\u{115f}' | '\u{1160}' | '\u{17b4}' | '\u{17b5}'
        | '\u{180b}'..='\u{180f}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
        | '\u{2060}'..='\u{206f}' | '\u{3164}' | '\u{fe00}'..='\u{fe0f}' | '\u{feff}'
        | '\u{ffa0}' | '\u{fff0}'..='\u{fff8}' | '\u{1bca0}'..='\u{1bca3}'
        | '\u{1d173}'..='\u{1d17a}' | '\u{e0000}'..='\u{e0fff}')
}

/// Byte length of the tag run that completes an emoji flag sequence at the
/// start of `rest` (tag letters, then the cancel tag), or 0.
fn emoji_tag_run_len(rest: &str) -> usize {
    let mut len = 0;
    for ch in rest.chars() {
        match ch {
            '\u{e0020}'..='\u{e007e}' => len += ch.len_utf8(),
            '\u{e007f}' if len > 0 => return len + ch.len_utf8(),
            _ => return 0,
        }
    }
    0
}

struct SpanRule {
    regex: Regex,
    secret_group: bool,
}

impl SpanRule {
    fn new(pattern: &str) -> Self {
        let regex = Regex::new(pattern).unwrap();
        let secret_group = regex.capture_names().any(|name| name == Some("secret"));
        Self {
            regex,
            secret_group,
        }
    }
}

struct CredentialRule {
    rule: SpanRule,
    is_credential: bool,
}

fn private_key_patterns() -> &'static [SpanRule] {
    static RULES: OnceLock<Vec<SpanRule>> = OnceLock::new();
    RULES.get_or_init(|| {
        vec![
            SpanRule::new(
                r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----.*?-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----",
            ),
            // A block with no end line, once at least one key-body line
            // follows the header; the line end after it sits outside `secret`.
            SpanRule::new(
                r#"(?P<secret>-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----(?:(?:\r?\n|(?:\\r)?\\n)[ \t]*(?:(?:Proc-Type|DEK-Info|Version|Comment|Hash):[^\r\n\\]*)?)*?(?:(?:\r?\n|(?:\\r)?\\n)[ \t]*[A-Za-z0-9+/=]{16,}[ \t]*)+)(?:\r?(?m:$)|["'\\…]|\.\.\.)"#,
            ),
        ]
    })
}

fn credential_patterns() -> &'static [CredentialRule] {
    static RULES: OnceLock<Vec<CredentialRule>> = OnceLock::new();
    RULES.get_or_init(|| {
        vec![
            // GitHub tokens: left-unanchored like the server's rule, so a
            // token glued onto a word by a stripped control character (a
            // removed BEL, `…\x07ghp_…`) is still matched.
            CredentialRule {
                rule: SpanRule::new(r"(?:github_pat_[A-Za-z0-9_]{20,}|gh[pousr]_[A-Za-z0-9]{20,})"),
                is_credential: true,
            },
            // AWS access-key IDs, anchored like the server's `aws_key` rule:
            // a boundary or a percent-encoded byte before the exact 20-char
            // shape. A letter removed from directly before `AKIA` stays
            // unmatched on purpose — redacting `AKIA` mid-word would destroy
            // ordinary uppercase text irreversibly.
            CredentialRule {
                rule: SpanRule::new(
                    r"(?:\b|%[0-9A-Fa-f]{2})(?P<secret>(?:AKIA|ASIA)[0-9A-Z]{16})\b",
                ),
                is_credential: true,
            },
            // Vendor `sk-` keys, anchored like the server's `api_key` rule: a
            // letter right before the prefix means it ends a word (`task-…`),
            // so only that may not start a match. A `\b` would also skip a key
            // glued to `_`, a digit, a CJK character or a percent escape.
            CredentialRule {
                rule: SpanRule::new(
                    r"(?:^|[^A-Za-z]|%[0-9A-Fa-f]{2})(?P<secret>sk-[A-Za-z0-9_-]{16,})",
                ),
                is_credential: true,
            },
            // ai-memory's own keys: native `aim_`, web-session `ams_`, and
            // external mcp-auth `amk_`. Left-unanchored, like the server's
            // `ai_memory_key` rule: a 256-bit base64url tail after the prefix
            // makes a false positive negligible, so matching the prefix even
            // when a stripped control character or the visible tail of an ANSI
            // sequence glues a word onto it is safe and necessary.
            CredentialRule {
                rule: SpanRule::new(r"(?:aim|ams|amk)_[A-Za-z0-9_-]{40,}"),
                is_credential: true,
            },
            // Bearer tokens: keep `Bearer ` outside `secret` so merged spans
            // emit `Bearer [REDACTED CREDENTIAL]` when overlapping a typed key
            // and `Bearer [REDACTED]` otherwise.
            CredentialRule {
                rule: SpanRule::new(r"(?i)\bbearer\s+(?P<secret>[A-Za-z0-9._~+/=-]{8,})"),
                is_credential: false,
            },
        ]
    })
}

fn key_value_pattern() -> &'static Regex {
    static RULE: OnceLock<Regex> = OnceLock::new();
    RULE.get_or_init(|| {
        Regex::new(
            r#"(?i)\b(api[_-]?key|access[_-]?token|auth[_-]?token|token|secret|password)\b((?:\\*")?\s*[:=]\s*)(\\*"[^"\\\r\n]{6,}\\*"|'[^'\r\n]{6,}'|(?:\[REDACTED(?: CREDENTIAL| PRIVATE KEY)?\]|[^\s,;])+)"#,
        )
        .unwrap()
    })
}
