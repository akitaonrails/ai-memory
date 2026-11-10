//! Local credential scrub shared by external capture clients.
//!
//! The server sanitizer (`ai-memory-core::sanitize`) re-scrubs everything
//! these clients send; this copy keeps credentials out of the local spool,
//! queue and preview files written before delivery. It follows the same
//! rules for private keys and invisible characters: key blocks are matched
//! first, including PGP armor and a block cut off before its end line, and
//! invisible formatting characters do not hide a credential from the
//! patterns.

use regex::Regex;
use std::sync::OnceLock;

pub fn sanitize_external_text(input: &str) -> String {
    let mut offset = 0;
    let mut controls_removed = String::with_capacity(input.len());
    while let Some(ch) = input[offset..].chars().next() {
        let next = offset + ch.len_utf8();
        if ch == '\u{1f3f4}' {
            let tags = emoji_tag_run_len(&input[next..]);
            controls_removed.push_str(&input[offset..next + tags]);
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
            controls_removed.push(ch);
        }
        offset = next;
    }
    secret_patterns()
        .iter()
        .fold(controls_removed, |text, rule| {
            // Through invisible characters first, so a split credential is
            // replaced whole; then the plain pass, which still finds a match
            // that needs an invisible character as its word boundary.
            let text = if text.contains(is_default_ignorable) {
                redact_through_invisibles(&text, rule)
            } else {
                text
            };
            rule.regex
                .replace_all(&text, |captures: &regex::Captures<'_>| {
                    replacement(rule, captures)
                })
                .into_owned()
        })
}

fn replacement(rule: &RedactionRule, captures: &regex::Captures<'_>) -> String {
    let matched = captures.get(0).map_or("", |matched| matched.as_str());
    if captures.get(3).is_some_and(|value| {
        value.as_str().chars().count() < 6
            || [
                "[REDACTED]",
                "[REDACTED CREDENTIAL]",
                "[REDACTED PRIVATE KEY]",
            ]
            .contains(&value.as_str())
    }) {
        matched.to_owned()
    } else {
        let mut replacement = String::new();
        captures.expand(rule.replacement, &mut replacement);
        replacement
    }
}

/// Run `rule` over `text` with invisible characters removed and replace each
/// match's span in `text`, invisible characters inside it included.
fn redact_through_invisibles(text: &str, rule: &RedactionRule) -> String {
    let mut shadow = String::with_capacity(text.len());
    let mut origin = Vec::with_capacity(text.len());
    for (index, ch) in text.char_indices() {
        if !is_default_ignorable(ch) {
            shadow.push(ch);
            origin.extend(index..index + ch.len_utf8());
        }
    }
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    for captures in rule.regex.captures_iter(&shadow) {
        let Some(found) = captures.get(0) else {
            continue;
        };
        let start = origin.get(found.start()).copied().unwrap_or(text.len());
        let end = match found.end().checked_sub(1).and_then(|last| origin.get(last)) {
            Some(last) if found.end() > found.start() => last + 1,
            _ => start,
        };
        let replaced = replacement(rule, &captures);
        // A match the rule keeps stays byte for byte, invisible characters
        // and all.
        if start < copied || replaced == found.as_str() {
            continue;
        }
        output.push_str(&text[copied..start]);
        output.push_str(&replaced);
        copied = end;
    }
    output.push_str(&text[copied..]);
    output
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

struct RedactionRule {
    regex: Regex,
    replacement: &'static str,
}

fn secret_patterns() -> &'static [RedactionRule] {
    static RULES: OnceLock<Vec<RedactionRule>> = OnceLock::new();
    RULES.get_or_init(|| {
        vec![
            RedactionRule {
                regex: Regex::new(
                    r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----.*?-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----",
                )
                .unwrap(),
                replacement: "[REDACTED PRIVATE KEY]",
            },
            // A block with no end line, once at least one key-body line
            // follows the header; the line end after it is kept.
            RedactionRule {
                regex: Regex::new(
                    r#"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----(?:(?:\r?\n|(?:\\r)?\\n)[ \t]*(?:(?:Proc-Type|DEK-Info|Version|Comment|Hash):[^\r\n\\]*)?)*?(?:(?:\r?\n|(?:\\r)?\\n)[ \t]*[A-Za-z0-9+/=]{16,}[ \t]*)+(?P<end>\r?(?m:$)|["'\\…]|\.\.\.)"#,
                )
                .unwrap(),
                replacement: "[REDACTED PRIVATE KEY]${end}",
            },
            // GitHub tokens: left-unanchored like the server's rule, so a
            // token glued onto a word by a stripped control character (a
            // removed BEL, `…\x07ghp_…`) is still matched.
            RedactionRule {
                regex: Regex::new(
                    r"(?:github_pat_[A-Za-z0-9_]{20,}|gh[pousr]_[A-Za-z0-9]{20,})",
                )
                .unwrap(),
                replacement: "[REDACTED CREDENTIAL]",
            },
            // AWS access-key IDs, anchored like the server's `aws_key` rule:
            // a boundary or a percent-encoded byte before the exact 20-char
            // shape. A letter removed from directly before `AKIA` stays
            // unmatched on purpose — redacting `AKIA` mid-word would destroy
            // ordinary uppercase text irreversibly.
            RedactionRule {
                regex: Regex::new(r"(?P<pre>\b|%[0-9A-Fa-f]{2})(?:AKIA|ASIA)[0-9A-Z]{16}\b")
                    .unwrap(),
                replacement: "${pre}[REDACTED CREDENTIAL]",
            },
            // Vendor `sk-` keys, anchored like the server's `api_key` rule: a
            // letter right before the prefix means it ends a word (`task-…`),
            // so only that may not start a match. A `\b` would also skip a key
            // glued to `_`, a digit, a CJK character or a percent escape.
            RedactionRule {
                regex: Regex::new(r"(?P<pre>^|[^A-Za-z]|%[0-9A-Fa-f]{2})sk-[A-Za-z0-9_-]{16,}")
                    .unwrap(),
                replacement: "${pre}[REDACTED CREDENTIAL]",
            },
            // ai-memory's own keys: native `aim_`, web-session `ams_`, and
            // external mcp-auth `amk_`. Left-unanchored, like the server's
            // `ai_memory_key` rule: a 256-bit base64url tail after the prefix
            // makes a false positive negligible, so matching the prefix even
            // when a stripped control character or the visible tail of an ANSI
            // sequence glues a word onto it is safe and necessary.
            RedactionRule {
                regex: Regex::new(r"(?:aim|ams|amk)_[A-Za-z0-9_-]{40,}").unwrap(),
                replacement: "[REDACTED CREDENTIAL]",
            },
            RedactionRule {
                regex: Regex::new(r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=-]{8,}").unwrap(),
                replacement: "$1[REDACTED]",
            },
            RedactionRule {
                regex: Regex::new(
                    r#"(?i)\b(api[_-]?key|access[_-]?token|auth[_-]?token|token|secret|password)\b((?:\\*")?\s*[:=]\s*)(\\*"[^"\\\r\n]{6,}\\*"|'[^'\r\n]{6,}'|(?:\[REDACTED(?: CREDENTIAL| PRIVATE KEY)?\]|[^\s,;])+)"#,
                )
                .unwrap(),
                replacement: "$1$2[REDACTED]",
            },
        ]
    })
}
