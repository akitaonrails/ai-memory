//! Privacy strip — redacts secrets before they reach durable storage.
//!
//! Two surfaces live here:
//!
//! - [`Sanitizer`] is a stateful, config-driven scrubber. Build it once
//!   at startup from `SanitizeConfig` and pass `Arc<Sanitizer>` to every
//!   subsystem that writes user-facing text to disk (hook ingress,
//!   consolidator, wiki writer, MCP `memory_handoff_begin`).
//! - [`Sanitized<T>`] is the typed boundary: the *only* way to construct
//!   one is through [`Sanitized::new`], which forces the caller to hand
//!   over an already-scrubbed value. Without this, you can't accidentally
//!   persist raw text by skipping the sanitizer.
//!
//! ## What we redact
//!
//! Built-in patterns cover bearer tokens, vendor-prefixed API keys
//! (Anthropic / OpenAI / OpenRouter sk-…, Stripe sk_live_/rk_live_…,
//! all GitHub token prefixes ghp_/gho_/ghu_/ghs_/ghr_ and fine-grained
//! github_pat_…, Google AIza… and AQ.Ab… plus OAuth refresh tokens 1//…, Meta /
//! Facebook Graph EAA…, Telegram bot tokens, GoHighLevel pit-…, Slack
//! xoxb/xoxp…, AWS AKIA/ASIA…), PEM-bracketed private
//! keys, URL-embedded credentials (`postgres://user:pass@host`), and
//! dotless base64 JSON tokens (Cloudflare tunnel tokens), and anything
//! matching the generic `*_(KEY|KEY_ID|TOKEN|SECRET|PASSWORD|CREDENTIAL|
//! PEPPER|SALT|…)=value` shape. Operators can extend the list via
//! `[sanitize].extra_patterns` and exempt substrings via
//! `[sanitize].allowlist` — the allowlist is checked *per match*, so a
//! pattern still runs but an allowlisted span survives unchanged.
//!
//! Private keys come first: complete PEM and PGP armor blocks, and a block
//! cut off before its end line (`head` of a key file, a truncated tool
//! result) as long as at least one key-body line follows the header. A
//! sentence that only names a header has no body line and survives. The
//! allowlist never exempts a private key.
//!
//! Invisible formatting characters (Unicode `Default_Ignorable_Code_Point`:
//! zero-width spaces and joiners, soft hyphens, variation selectors, …) do
//! not hide a credential from the patterns. When the text carries any, each
//! pattern also runs over a copy with them removed and the match is mapped
//! back to the original span, invisible characters included. Text that
//! matches nothing is stored byte for byte, so emoji sequences and scripts
//! that need joiners (Persian, the Indic scripts) are never altered.
//!
//! Terminal escape sequences, NUL and bidi overrides are stripped rather
//! than redacted: they are not secrets, but stored text is replayed to a
//! terminal by the CLI, and a NUL makes the markdown file binary. Tag
//! characters outside an emoji flag sequence, a byte-order mark after the
//! start of the text, and the invisible math operators are stripped too:
//! captured prose has no use for them, and they can carry text a reader
//! never sees.
//!
//! ## What we deliberately do not catch
//!
//! Standalone high-entropy strings (e.g. a 32-char random hex) cannot
//! be safely redacted without knowing their structure — too many false
//! positives. Operators who care about that level of paranoia should
//! add a custom pattern via `extra_patterns`.
//!
//! Look-alike characters (full-width `ｓｋ－…`, other confusables) are not
//! folded: matching does not apply NFKC normalization.

use std::sync::Arc;

use regex::Regex;
use tracing::debug;

use crate::NewObservation;

/// Universal durable-body ceiling for lifecycle observations. Event adapters
/// may impose smaller limits, but no sanitized observation can cross the store
/// boundary above 16 KiB.
pub const OBSERVATION_BODY_MAX_BYTES: usize = 16 * 1024;

/// ANSI/VT escape sequences: CSI (`ESC [ ... final`), OSC (`ESC ] ... BEL` or
/// `ESC \\`), and the two-character forms. Removed whole, so a colour code
/// does not leave `[31m` behind as text.
const ESCAPE_SEQUENCES: &str =
    r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]";

/// C0 and C1 controls except tab, newline and carriage return, DEL, and the
/// bidi overrides that let stored text render as something else.
const fn is_stripped_control(c: char) -> bool {
    matches!(c,
        '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}'
        | '\u{7f}'..='\u{9f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Characters whose only realistic use in captured text is hiding it: the
/// tag block (also used by emoji flag sequences, which [`emoji_tag_run_len`]
/// keeps), the invisible math operators, and a byte-order mark anywhere but
/// the start of the text (`U+2060` replaced it as a zero-width no-break
/// space).
const fn is_hidden_text_char(offset: usize, c: char) -> bool {
    matches!(c, '\u{2061}'..='\u{2064}' | '\u{e0000}'..='\u{e007f}')
        || (c == '\u{feff}' && offset > 0)
}

/// Unicode `Default_Ignorable_Code_Point` (UAX #44, `DerivedCoreProperties`):
/// characters that render as nothing. Matching ignores them; storage keeps
/// them unless [`is_stripped_control`] or [`is_hidden_text_char`] says
/// otherwise.
const fn is_default_ignorable(c: char) -> bool {
    matches!(c,
        '\u{ad}' | '\u{34f}' | '\u{61c}' | '\u{115f}' | '\u{1160}' | '\u{17b4}' | '\u{17b5}'
        | '\u{180b}'..='\u{180f}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
        | '\u{2060}'..='\u{206f}' | '\u{3164}' | '\u{fe00}'..='\u{fe0f}' | '\u{feff}'
        | '\u{ffa0}' | '\u{fff0}'..='\u{fff8}' | '\u{1bca0}'..='\u{1bca3}'
        | '\u{1d173}'..='\u{1d17a}' | '\u{e0000}'..='\u{e0fff}')
}

/// Byte length of the tag characters that close an emoji flag sequence
/// (`U+1F3F4`, tag letters, `U+E007F` cancel tag) at the start of `rest`, or
/// 0 when `rest` does not start with a well-formed run.
fn emoji_tag_run_len(rest: &str) -> usize {
    let mut len = 0;
    for c in rest.chars() {
        match c {
            '\u{e0020}'..='\u{e007e}' => len += c.len_utf8(),
            '\u{e007f}' if len > 0 => return len + c.len_utf8(),
            _ => return 0,
        }
    }
    0
}

/// `text` with every [`is_default_ignorable`] character removed, plus the
/// original byte offset of each byte kept, so a match found in the shadow
/// maps back to the span it covers in `text`.
struct Shadow {
    text: String,
    origin: Vec<usize>,
}

impl Shadow {
    fn new(text: &str) -> Self {
        let mut shadow = String::with_capacity(text.len());
        let mut origin = Vec::with_capacity(text.len());
        for (index, c) in text.char_indices() {
            if is_default_ignorable(c) {
                continue;
            }
            shadow.push(c);
            origin.extend(index..index + c.len_utf8());
        }
        Self {
            text: shadow,
            origin,
        }
    }

    /// The span of `text` a shadow match `start..end` covers: from its first
    /// visible character through its last, invisible characters between them
    /// included.
    fn original_span(&self, text_len: usize, start: usize, end: usize) -> (usize, usize) {
        let from = self.origin.get(start).copied().unwrap_or(text_len);
        let to = match end.checked_sub(1).and_then(|last| self.origin.get(last)) {
            Some(last) if end > start => last + 1,
            _ => from,
        };
        (from, to)
    }
}

/// Compile-time list of redaction patterns. Order is intentional:
/// more-specific patterns first. False positives are acceptable —
/// better to redact a stray hash than to leak a credential.
const BUILTIN_PATTERNS: &[(&str, &str)] = &[
    // Private keys run first, while their base64 body lines are still intact
    // for the token rules below to leave alone.
    //
    // A complete PEM or PGP armor block (`… PRIVATE KEY BLOCK-----` is the PGP
    // form) — multi-line, lazy match.
    (
        r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----.*?-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----",
        "private_key",
    ),
    // A block with no end line: the head of a key file, or a tool result cut
    // short before the hook saw it. It needs at least one key-body line (16+
    // base64 characters filling the rest of the line) after the header,
    // optionally behind armor headers (`Proc-Type:`, `Version:`, …) and blank
    // lines, so prose that only names the header keeps it. Lines break on a
    // real newline or a JSON-escaped one. The line end after the last body
    // line sits outside the `secret` group: it has to be there, but it is not
    // redacted.
    (
        r#"(?P<secret>-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----(?:(?:\r?\n|(?:\\r)?\\n)[ \t]*(?:(?:Proc-Type|DEK-Info|Version|Comment|Hash):[^\r\n\\]*)?)*?(?:(?:\r?\n|(?:\\r)?\\n)[ \t]*[A-Za-z0-9+/=]{16,}[ \t]*)+)(?:\r?(?m:$)|["'\\…]|\.\.\.)"#,
        "private_key",
    ),
    // Bearer-style tokens.
    (r#"(?i)bearer\s+[A-Za-z0-9._\-+/=]{16,}"#, "bearer_token"),
    // Vendor-prefixed API keys.
    (r"sk-[A-Za-z0-9_\-]{16,}", "api_key"),
    // Stripe secret *and* restricted keys. `rk_live_` is scoped rather than
    // full-access, but the scope is operator-chosen and routinely includes
    // charges/refunds — not meaningfully safer than `sk_live_`.
    (r"(?:sk|rk)_live_[A-Za-z0-9_\-]{16,}", "stripe_key"),
    // Every GitHub token prefix, not only personal-access: `gho_` (OAuth —
    // what `gh auth login` stores on disk), `ghu_` (user-to-server),
    // `ghs_` (server-to-server / Actions), `ghr_` (refresh).
    (r"gh[pousr]_[A-Za-z0-9]{20,}", "github_token"),
    (r"github_pat_[A-Za-z0-9_]{20,}", "github_token"),
    // AWS access-key IDs: long-lived (AKIA) and STS temporary (ASIA).
    //
    // Anchored to the exact published format — a 4-character prefix plus
    // sixteen more, twenty in total — rather than an open `{12,}` tail.
    // `ASIA` is also an English word, and an open tail redacted ordinary
    // uppercase text: `ASIAPACIFICREGION` and `ASIAEAST1CLUSTER` were both
    // destroyed. That is worse than it sounds here, because the strip runs
    // BEFORE storage and is irreversible: the observation loses the text with
    // no error and no way to recover it. The word boundaries keep a real key
    // from being missed when it sits inside punctuation.
    (r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", "aws_key"),
    // Naked Google / Gemini API keys (legacy standard-key shape).
    (r"AIza[A-Za-z0-9_\-]{30,}", "google_api_key"),
    // Gemini authorization ("auth") keys. AI Studio has issued these instead
    // of `AIza…` keys since 2026-05-28: `AQ.Ab` followed by a base64url tail
    // (48 characters on a key issued 2026-10). The `.Ab` anchor keeps
    // ordinary text such as "AQ.A" headings or `AQ.` abbreviations intact;
    // the open tail floor matches the AIza rule's.
    (r"AQ\.Ab[A-Za-z0-9_\-]{30,}", "google_api_key"),
    // Google OAuth refresh tokens. Longer-lived than the AIza keys above:
    // they mint fresh access tokens until explicitly revoked, so a leaked
    // one outlives the session it came from.
    (r"1//[0-9A-Za-z_\-]{20,}", "google_oauth"),
    // Meta / Facebook Graph API access tokens (ad accounts, pages,
    // business management).
    (r"EAA[A-Za-z0-9]{20,}", "meta_token"),
    // Telegram bot tokens: <bot-id>:<secret>. Grants full control of the bot,
    // including reading every message it can see. Two branches on purpose:
    //  - `AA…` is the shape every issued token has taken, left open-ended so a
    //    future length change cannot silently retire the rule.
    //  - the second branch matches the shape Telegram's own docs publish
    //    (`123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11` — 6-digit id, no `AA`),
    //    length-anchored because without the `AA` anchor a bare `\d+:[\w-]+`
    //    also matches timestamps, ratios, `host:port` maps, SRT cues and
    //    `<short-sha>:<hex>` pairs.
    // Thanks to @tahazarif10 for spotting that the documented example fell
    // outside the original `\d{8,10}:AA…` form.
    (
        r"\b\d{6,10}:(?:AA[A-Za-z0-9_\-]{30,}|[A-Za-z0-9_\-]{34,35})\b",
        "telegram_token",
    ),
    // GoHighLevel Private Integration Tokens. The `pit-` prefix is what the
    // vendor documents (their MCP guide shows `Bearer pit-your-token`); the
    // tail is NOT documented anywhere, and every token observed in the wild
    // carries a UUID. Anchoring on the UUID shape rather than a permissive
    // tail is deliberate: `pit-` is also an English fragment, so
    // `pit-[A-Za-z0-9\-]{20,}` would redact "pit-stop-strategy-analysis".
    // These tokens do not expire until manually revoked.
    (
        r"pit-[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
        "ghl_token",
    ),
    // Slack tokens (bot/user/admin/app-level/refresh).
    (r"xox[abprs]-[A-Za-z0-9\-]{10,}", "slack_token"),
    (r"xapp-[A-Za-z0-9\-]{10,}", "slack_token"),
    // JWTs (three base64url segments separated by dots).
    (
        r"eyJ[A-Za-z0-9_\-]{16,}\.[A-Za-z0-9_\-]{16,}\.[A-Za-z0-9_\-]{16,}",
        "jwt",
    ),
    // A base64 JSON object with no dots: Cloudflare tunnel tokens
    // (`cloudflared service install eyJ…` is how the dashboard hands one out)
    // and other opaque `{"…` blobs the JWT rule's three segments miss. `eyJ` is
    // only base64 for `{"`, so the 40-character floor is what separates a
    // credential from a short encoded fragment; config that long is pasted as
    // JSON, not base64.
    (r"eyJ[A-Za-z0-9_\-+/]{40,}={0,2}", "base64_json_token"),
    // URL-embedded credentials: scheme://user:pass@host.
    (
        r"[a-zA-Z][a-zA-Z0-9+\-.]*://[^:/\s]+:[^@\s]+@[^\s]+",
        "url_credentials",
    ),
    // Auth-bearing HTTP headers carrying an opaque value: AWS SigV4's
    // `X-Amz-Security-Token`, `X-Api-Key`, GitLab's `Private-Token`, Azure's
    // `Ocp-Apim-Subscription-Key`. Neither of the rules above reaches these:
    // the `bearer\s+` rule needs the literal scheme keyword, and the generic
    // env-var rule below needs an `UPPER_SNAKE_TOKEN=` shape that a
    // kebab-case header name never matches. Tool output echoing a curl
    // invocation is the usual way they reach capture.
    //
    // A `key` or `token` suffix alone does not imply a secret.
    // `Idempotency-Key`, `Continuation-Token` and storage partition keys use
    // it for values that carry no credential and stay useful when reading
    // captured output, so that suffix must be qualified by an auth word.
    // Unambiguous words (`password`, `secret`, `authorization`) stand alone.
    //
    // The value floor keeps short literals such as CORS
    // `Access-Control-Allow-Credentials: true` intact. It is not what
    // protects an already-redacted value: `[REDACTED]` starts with `[`,
    // which the value character class excludes outright.
    (
        r#"(?i)\b[A-Za-z0-9_-]*(?:authentication|authorization|credentials?|password|passwd|apikey|accountkey|authtoken|[a-z0-9]*(?:api|auth|access|secret|security|private|session|refresh|client|consumer|subscription|app|bearer)-(?:key|token))"?\s*[=:]\s*(?:(?:basic|bearer|digest|token|apikey)\s+)?"?[A-Za-z0-9._~+/=-]{8,}"#,
        "auth_header",
    ),
    // Provider-specific env-var assignments (kept explicit for clarity
    // and so that bare `OPENAI_API_KEY=anything-at-all` still triggers
    // even without `sk-` shape).
    (
        r#"(?i)(ANTHROPIC_API_KEY|OPENAI_API_KEY|OPENROUTER_API_KEY|VOYAGE_API_KEY|MISTRAL_API_KEY|GROQ_API_KEY|HF_TOKEN|HUGGINGFACE_TOKEN|AWS_(SECRET_)?ACCESS_KEY[A-Z_]*|GITHUB_TOKEN|GH_TOKEN|GITLAB_TOKEN|GOOGLE_API_KEY|GEMINI_API_KEY|OLLAMA_API_KEY)"?\s*[=:]\s*\S+"#,
        "env_secret",
    ),
    // Generic env-var catch-all: any *_KEY / *_TOKEN / *_SECRET /
    // *_PASSWORD / *_CREDENTIAL[S] / *_PRIVATE_KEY assignment.
    (
        r#"(?i)\b[A-Z][A-Z0-9_]*_(KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|CREDENTIALS|PRIVATE_KEY)"?\s*[=:]\s*\S+"#,
        "env_secret",
    ),
    // The suffixes one step outside those: `*_KEY_ID` (the other half of an
    // S3 pair, e.g. `LITESTREAM_ACCESS_KEY_ID`), `*_PASSPHRASE`,
    // `*_SIGNING_KEY`, and `*_PEPPER` / `*_SALT`, which turn a stolen hash
    // table into a usable one. Uppercase only, the environment-variable
    // convention: as code identifiers (`api_key_id`, `password_salt`) they are
    // ordinary column and field names, and redaction is irreversible.
    (
        r#"\b[A-Z][A-Z0-9_]*_(KEY_ID|PASSPHRASE|SIGNING_KEY|PEPPER|SALT)"?\s*[=:]\s*\S+"#,
        "env_secret",
    ),
    // Filesystem paths that commonly contain credentials. The separator
    // class is `[\\/]` and an optional `X:` drive prefix is accepted so a
    // Windows agent echoing `C:\Users\alice\.ssh\id_rsa` is redacted the
    // same way as `/home/user/.ssh/id_rsa`. Case-insensitive because NTFS
    // is; Unix paths stay covered because they still start with `/`.
    (
        r"(?i)(?:[A-Za-z]:)?(?:[\\/][^\\/\s]+)*[\\/]\.ssh(?:[\\/][^\s]+)?",
        "credential_path",
    ),
    (
        r"(?i)(?:[A-Za-z]:)?(?:[\\/][^\\/\s]+)*[\\/]\.aws(?:[\\/][^\s]+)?",
        "credential_path",
    ),
    (
        r"(?i)(?:[A-Za-z]:)?(?:[\\/][^\\/\s]+)*[\\/]\.kube(?:[\\/][^\s]+)?",
        "credential_path",
    ),
    (
        r"(?i)(?:[A-Za-z]:)?(?:[\\/][^\\/\s]+)*[\\/]\.config[\\/]gcloud(?:[\\/][^\s]+)?",
        "credential_path",
    ),
    (
        r"(?i)(?:[A-Za-z]:)?(?:[\\/][^\\/\s]+)*[\\/]\.gnupg(?:[\\/][^\s]+)?",
        "credential_path",
    ),
];

/// Stateful sanitizer. Cheap to clone — wraps an `Arc` of compiled
/// patterns. Construct once at startup, then pass everywhere by clone.
#[derive(Clone)]
pub struct Sanitizer {
    inner: Arc<SanitizerInner>,
}

struct SanitizerInner {
    patterns: Vec<Pattern>,
    allowlist: Vec<String>,
    escapes: Regex,
}

struct Pattern {
    re: Regex,
    label: &'static str,
    /// A built-in rule whose match must include context that is not itself
    /// secret (a line end) redacts only its `secret` capture group.
    secret_group: bool,
}

/// The one label the allowlist never exempts: no project codename belongs
/// inside a private key, and a short entry would otherwise match key bodies.
const PRIVATE_KEY_LABEL: &str = "private_key";

impl std::fmt::Debug for Sanitizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sanitizer")
            .field("patterns", &self.inner.patterns.len())
            .field("allowlist", &self.inner.allowlist.len())
            .finish()
    }
}

/// User-tunable sanitizer settings. Mirrors the `[sanitize]` section
/// of `config.toml`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SanitizeConfig {
    /// Additional regex patterns to redact. Compiled once at startup;
    /// invalid regex aborts startup with a clear error.
    pub extra_patterns: Vec<String>,
    /// Substrings that should *never* be redacted, even if a pattern
    /// matches them. Useful when a project codename collides with the
    /// generic env-var catch-all (e.g. "PROJECT_TOKEN" the user wants
    /// to keep visible).
    pub allowlist: Vec<String>,
}

impl Sanitizer {
    /// Build a sanitizer from built-in patterns plus the operator's
    /// extras. Returns an error if any extra pattern fails to compile.
    ///
    /// # Errors
    /// Returns [`regex::Error`] when an entry in `extra_patterns` is
    /// not a valid regex.
    pub fn new(cfg: &SanitizeConfig) -> Result<Self, regex::Error> {
        let mut patterns = Vec::with_capacity(BUILTIN_PATTERNS.len() + cfg.extra_patterns.len());
        for (p, label) in BUILTIN_PATTERNS {
            let re = Regex::new(p)?;
            let secret_group = re.capture_names().any(|name| name == Some("secret"));
            patterns.push(Pattern {
                re,
                label,
                secret_group,
            });
        }
        for p in &cfg.extra_patterns {
            patterns.push(Pattern {
                re: Regex::new(p)?,
                label: "custom",
                secret_group: false,
            });
        }
        Ok(Self {
            inner: Arc::new(SanitizerInner {
                patterns,
                // A blank entry is a substring of every match and would turn
                // redaction off entirely. `Config::load` warns about it.
                allowlist: cfg
                    .allowlist
                    .iter()
                    .filter(|entry| !entry.trim().is_empty())
                    .cloned()
                    .collect(),
                escapes: Regex::new(ESCAPE_SEQUENCES)?,
            }),
        })
    }

    /// Built-in-only sanitizer (no operator extras, no allowlist).
    /// Convenient for tests and zero-config callers.
    #[must_use]
    pub fn builtin() -> Self {
        Self::new(&SanitizeConfig::default()).expect("built-in patterns compile")
    }

    /// Scrub a single string. Each match is replaced with
    /// `[REDACTED:<kind>]` unless the matched substring contains an
    /// allowlist entry, in which case it is left alone.
    #[must_use]
    pub fn scrub(&self, input: &str) -> String {
        // Before the redaction passes: an escape inside a secret would
        // otherwise split it out of reach of every pattern below.
        let mut out = self.strip_control(input);
        for pattern in &self.inner.patterns {
            // When invisible characters are present, the pass that looks
            // through them goes first, so a split secret is redacted whole
            // rather than losing only the part before the split. The plain
            // pass then still finds a match that needs an invisible character
            // as its word boundary.
            if out.contains(is_default_ignorable) {
                let shadow = Shadow::new(&out);
                out = self.redact(&out, Some(&shadow), pattern);
            }
            out = self.redact(&out, None, pattern);
        }
        out
    }

    /// One pattern over `text`, or over `shadow` with each match mapped back
    /// to the span of `text` it covers.
    fn redact(&self, text: &str, shadow: Option<&Shadow>, pattern: &Pattern) -> String {
        let view = shadow.map_or(text, |shadow| shadow.text.as_str());
        let spans: Vec<regex::Match<'_>> = if pattern.secret_group {
            pattern
                .re
                .captures_iter(view)
                .filter_map(|caps| caps.name("secret").or_else(|| caps.get(0)))
                .collect()
        } else {
            pattern.re.find_iter(view).collect()
        };
        let mut out = String::new();
        let mut copied = 0;
        let mut redacted = false;
        for m in spans {
            if self.allowlisted(pattern.label, m.as_str()) {
                continue;
            }
            let (start, end) = match shadow {
                Some(shadow) => shadow.original_span(text.len(), m.start(), m.end()),
                None => (m.start(), m.end()),
            };
            if start < copied {
                continue;
            }
            debug!(pattern = pattern.re.as_str(), "sanitize: redacted match");
            out.push_str(&text[copied..start]);
            out.push_str("[REDACTED:");
            out.push_str(pattern.label);
            out.push(']');
            copied = end;
            redacted = true;
        }
        if !redacted {
            return text.to_string();
        }
        out.push_str(&text[copied..]);
        out
    }

    /// Whether an allowlist entry exempts `matched`. Compared with invisible
    /// characters removed, so they neither break nor fake an entry.
    fn allowlisted(&self, label: &str, matched: &str) -> bool {
        if label == PRIVATE_KEY_LABEL || self.inner.allowlist.is_empty() {
            return false;
        }
        let visible: String;
        let matched = if matched.contains(is_default_ignorable) {
            visible = matched
                .chars()
                .filter(|c| !is_default_ignorable(*c))
                .collect();
            visible.as_str()
        } else {
            matched
        };
        self.inner
            .allowlist
            .iter()
            .any(|entry| matched.contains(entry.as_str()))
    }

    /// Drop terminal escape sequences, the control characters that have no
    /// place in a page, and characters that hide text.
    ///
    /// Captured text is replayed to a terminal by `ai-memory read-page` and
    /// `search`, where an escape rewrites the screen or the window title, and
    /// a bidi override reverses what the reader sees. A NUL additionally
    /// makes the markdown file binary, which costs it `grep` and git diffs.
    /// Tag characters spell out text no reader sees; the ones that finish an
    /// emoji flag (England, Scotland, Wales) stay.
    fn strip_control(&self, input: &str) -> String {
        let stripped = self.inner.escapes.replace_all(input, "");
        if !stripped
            .char_indices()
            .any(|(offset, c)| is_stripped_control(c) || is_hidden_text_char(offset, c))
        {
            return stripped.into_owned();
        }
        let mut out = String::with_capacity(stripped.len());
        let mut offset = 0;
        while let Some(c) = stripped[offset..].chars().next() {
            let next = offset + c.len_utf8();
            if c == '\u{1f3f4}' {
                let tags = emoji_tag_run_len(&stripped[next..]);
                out.push_str(&stripped[offset..next + tags]);
                offset = next + tags;
                continue;
            }
            if !is_stripped_control(c) && !is_hidden_text_char(offset, c) {
                out.push(c);
            }
            offset = next;
        }
        out
    }
}

impl Default for Sanitizer {
    fn default() -> Self {
        Self::builtin()
    }
}

/// Marker carried by every value that has passed through the privacy
/// strip. The wrapped value is private and reachable only via
/// [`Sanitized::inner`] / [`Sanitized::into_inner`].
#[derive(Debug, Clone)]
pub struct Sanitized<T>(T);

impl<T> Sanitized<T> {
    /// Borrow the inner sanitized value.
    pub fn inner(&self) -> &T {
        &self.0
    }
    /// Consume and return the inner sanitized value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl Sanitized<NewObservation> {
    /// Apply the privacy strip to an observation's title + body, then enforce
    /// the universal durable-body ceiling and the title display cap after
    /// redaction.
    ///
    /// Scrub-then-truncate, for both fields, in that order. Truncating first
    /// (as `ai-memory-hooks::payload` used to do for `title_hint`, before
    /// #980) can cut a secret in half before the sanitizer ever sees it: the
    /// stored prefix is often too short to match a pattern, so the fragment
    /// lands in the title unredacted. Scrubbing first also means a
    /// `[REDACTED:…]` marker is already in place before the length cap runs,
    /// same as the body.
    #[must_use]
    pub fn new(mut obs: NewObservation, sanitizer: &Sanitizer) -> Self {
        obs.title = truncate_for_title(&sanitizer.scrub(&obs.title));
        obs.body =
            truncate_utf8_bytes_head_tail(&sanitizer.scrub(&obs.body), OBSERVATION_BODY_MAX_BYTES);
        Self(obs)
    }
}

/// Cap a (single-line) title to at most 80 displayed characters, appending an
/// ellipsis when the input is longer. Applied by [`Sanitized::new`] to
/// `NewObservation::title` — after redaction, not before, so a secret that
/// straddles the cutoff is never stored half-redacted (see #980). Callers
/// that build a title hint upstream of sanitization (`ai-memory-hooks`) must
/// reduce it to a single line themselves; this function does not strip
/// newlines, mirroring [`truncate_utf8_bytes`], which does not either.
///
/// Known cosmetic gap: running after redaction fixes the security bug
/// (the secret itself is already gone by the time this runs), but a plain
/// char-count cutoff can still land inside a `[REDACTED:…]` marker the
/// scrubber just inserted, truncating it to something like `[REDACTE…`. A
/// title long enough to need this cap, ending in a marker, reproduces it
/// (see the `issue_980_user_prompt_title_is_sanitized_before_truncated` test
/// in `ai-memory-hooks::router`, which observes exactly this). Left
/// unhandled deliberately: no secret material leaks, and centering the cap
/// on marker boundaries would add real complexity for a display nicety.
#[must_use]
pub fn truncate_for_title(s: &str) -> String {
    const MAX: usize = 80;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let mut buf: String = s.chars().take(MAX - 1).collect();
        buf.push('…');
        buf
    }
}

/// Truncate text to at most `max` UTF-8 bytes, reserving the ellipsis inside
/// the requested cap and never splitting a code point.
#[must_use]
pub fn truncate_utf8_bytes(input: &str, max: usize) -> String {
    if input.len() <= max {
        return input.to_string();
    }
    if max < '…'.len_utf8() {
        return String::new();
    }
    let limit = max - '…'.len_utf8();
    let mut end = 0;
    for (index, character) in input.char_indices() {
        let next = index + character.len_utf8();
        if next > limit {
            break;
        }
        end = next;
    }
    let mut output = String::with_capacity(max);
    output.push_str(&input[..end]);
    output.push('…');
    output
}

/// Truncate text to at most `max` UTF-8 bytes, keeping **both** the head
/// and the tail so the middle can be elided.
///
/// The plain [`truncate_utf8_bytes`] keeps only the head, which loses the
/// tail of a long tool output (e.g. a 50 KB file read). When the LLM
/// consolidator later reads the observation body it sees an incomplete
/// picture and may produce less accurate summaries. This variant splits
/// the budget: head gets `max/2`, tail gets `max/2`, and a truncation
/// marker is inserted between them so the boundary is visible.
///
/// Never splits a code point. Falls back to [`truncate_utf8_bytes`] when
/// the budget is too small to split meaningfully (under 64 bytes).
#[must_use]
pub fn truncate_utf8_bytes_head_tail(input: &str, max: usize) -> String {
    if input.len() <= max {
        return input.to_string();
    }
    // For small budgets the head-tail split produces tiny fragments with
    // a marker that is longer than the content. Fall back to head-only.
    if max < 64 {
        return truncate_utf8_bytes(input, max);
    }
    // Reserve bytes for the truncation marker: "\n...[truncated N bytes]...\n"
    // where N is at most ~10 digits. Use a fixed 48-byte reservation —
    // generous enough for the marker plus newlines, small enough to leave
    // meaningful head/tail budgets.
    const MARKER_RESERVE: usize = 48;
    let usable = max.saturating_sub(MARKER_RESERVE);
    let half = usable / 2;

    // Walk forward for the head.
    let mut head_end = 0;
    for (index, character) in input.char_indices() {
        let next = index + character.len_utf8();
        if next > half {
            break;
        }
        head_end = next;
    }

    // Walk backward from the end for the tail.
    let total = input.len();
    let tail_start_target = total.saturating_sub(half);
    let mut tail_start = total;
    for (index, _) in input.char_indices().rev() {
        if index <= tail_start_target {
            tail_start = index;
            break;
        }
        tail_start = index;
    }

    let omitted = tail_start.saturating_sub(head_end);
    let mut output = String::with_capacity(max);
    output.push_str(&input[..head_end]);
    output.push_str(&format!("\n...[truncated {omitted} bytes]...\n"));
    output.push_str(&input[tail_start..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ObservationKind, ProjectId, SessionId, WorkspaceId};

    fn s() -> Sanitizer {
        Sanitizer::builtin()
    }

    #[test]
    fn scrubs_the_suffixes_one_step_outside_key_and_token() {
        // Fixtures are FAKE shapes, not real values.
        for (input, secret) in [
            (
                "LITESTREAM_ACCESS_KEY_ID=fake0000fake0000fake0000fake0000",
                "fake0000fake0000fake0000fake0000",
            ),
            (
                "AI_MEMORY_AUTH_TOKEN_PEPPER=fakepepperfakepepperfakepepper00",
                "fakepepperfakepepperfakepepper00",
            ),
            ("PASSWORD_SALT: fakesaltfakesalt00", "fakesaltfakesalt00"),
            ("GPG_PASSPHRASE=correct-horse-fake", "correct-horse-fake"),
            ("RELEASE_SIGNING_KEY=fakesigningkey000", "fakesigningkey000"),
        ] {
            let out = s().scrub(input);
            assert!(!out.contains(secret), "{input} -> {out}");
            assert!(out.contains("[REDACTED:env_secret]"), "{input} -> {out}");
        }
    }

    #[test]
    fn lowercase_identifiers_with_the_new_suffixes_survive() {
        // Code, not environment: the new suffixes are uppercase-only.
        for keep in [
            "let api_key_id = row.get(0);",
            "pub password_salt: String,",
            "struct Row { user_key_id: u64 }",
            "cache_key_id: abc123",
        ] {
            assert_eq!(s().scrub(keep), keep);
        }
    }

    #[test]
    fn scrubs_a_dotless_base64_json_token() {
        // The shape of a Cloudflare tunnel token: base64 of `{"a":"…","t":"…"}`
        // with no dots, handed out as a shell command rather than an assignment.
        let token = "eyJhIjoiRkFLRUZBS0VGQUtFRkFLRUZBS0VGQUtFIiwidCI6IkZBS0VGQUtFIn0=";
        let out = s().scrub(&format!("cloudflared service install {token}"));
        assert!(!out.contains(token), "{out}");
        assert_eq!(
            out,
            "cloudflared service install [REDACTED:base64_json_token]"
        );
    }

    #[test]
    fn ordinary_identifiers_survive_the_wider_rules() {
        for keep in [
            // A git SHA, a SHA-256 digest, a UUID and a version string.
            "commit 1f0775428a9e4b1c2d3e4f5a6b7c8d9e0f1a2b3c",
            "sha256 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "request 3b58f2d3-c1ac-81b2-b387-dd04ff9a3d11",
            "version 2.4.2",
            // `eyJ` is only base64 for `{"`: a short run is not a credential.
            "payload eyJhIjoi",
            // The suffixes are words in prose, not assignments.
            "add a pinch of SALT and a KEY_ID column",
        ] {
            assert_eq!(s().scrub(keep), keep);
        }
    }

    #[test]
    fn scrubs_bearer_token() {
        let out = s().scrub("Authorization: Bearer abcdef0123456789ABCDEF0123456789");
        assert!(out.contains("[REDACTED:"));
        assert!(!out.contains("abcdef0123"));
    }

    #[test]
    fn scrubs_openrouter_key_via_sk_prefix() {
        let out = s().scrub("key=sk-or-v1-deadbeefcafebabe1234567890abcdef");
        assert!(out.contains("[REDACTED:"));
        assert!(!out.contains("deadbeef"));
    }

    #[test]
    fn scrubs_naked_google_api_key() {
        // Fixture is the AIzaSy… shape, NOT a real key. A previous
        // iteration of this file used a live value as the fixture; do not
        // do that — automated scanners (GitGuardian, Google's own) will
        // pick it up and you'll spend an hour rotating credentials.
        //
        // Kept to 36 characters on purpose. At 40 it also matched the shape
        // of an AWS *secret* access key (40 chars of the base64 alphabet),
        // and GitHub push protection blocked pushes of any branch carrying
        // this file. Google's rule only needs `AIza` plus 30, so the shorter
        // fixture still exercises it.
        let out = s().scrub("the key AIzaSyFAKEfake0123456789abcdefghijkl is leaked");
        assert!(out.contains("[REDACTED:"));
        assert!(!out.contains("AIzaSy"));
    }

    #[test]
    fn scrubs_naked_gemini_auth_key() {
        // SHAPE only (`AQ.Ab` + base64url tail), never a live value — same
        // rule as the AIzaSy… fixture above.
        let out = s().scrub("export it: AQ.AbFAKEfake0123456789abcdefghijklmn-_FAKE done");
        assert!(out.contains("[REDACTED:"), "{out}");
        assert!(!out.contains("AQ.AbFAKE"), "{out}");
        assert!(out.ends_with(" done"), "{out}");
    }

    #[test]
    fn leaves_short_aq_prefixed_text_alone() {
        // Not a key: too short, or missing the `.Ab` anchor.
        for text in [
            "see AQ.Ab for details",
            "AQ.A section",
            "AQ.Xyz0123456789abcdefghijklmnopqrstuvwxyz",
        ] {
            assert_eq!(s().scrub(text), text);
        }
    }

    // Every fixture below is a SHAPE, never a live value — same rule as the
    // AIzaSy… test above, and the same `FAKE` convention used by the
    // `ghp_FAKE…` fixture in ai-memory-wiki. Keep them obviously synthetic
    // so credential scanners do not flag this repository.

    #[test]
    fn scrubs_all_github_token_prefixes() {
        // ghp_ was already covered; gho_/ghu_/ghs_/ghr_ were not, and gho_
        // is the prefix `gh auth login` writes to disk.
        for tok in [
            "ghp_FAKEfakeFAKEfakeFAKEfake012345678",
            "gho_FAKEfakeFAKEfakeFAKEfake012345678",
            "ghu_FAKEfakeFAKEfakeFAKEfake012345678",
            "ghs_FAKEfakeFAKEfakeFAKEfake012345678",
            "ghr_FAKEfakeFAKEfakeFAKEfake012345678",
        ] {
            let out = s().scrub(&format!("token={tok}"));
            assert!(out.contains("[REDACTED:"), "not redacted: {tok}");
            assert!(!out.contains("FAKEfake"), "leaked: {tok}");
        }
    }

    /// The strip runs before storage and cannot be undone, so a pattern that
    /// over-matches destroys captured content silently. Every other test here
    /// asserts that a secret IS redacted; these assert that ordinary text is
    /// NOT — the direction that was missing when `ASIA` shipped with an open
    /// tail and started eating uppercase identifiers.
    #[test]
    fn leaves_ordinary_text_untouched() {
        let s = s();
        for text in [
            // `ASIA` is an English word. These are the exact shapes an open
            // `(?:AKIA|ASIA)[0-9A-Z]{12,}` tail destroyed.
            "ASIAPACIFICREGION",
            "rollout to ASIAEAST1CLUSTER tonight",
            "ASIAPAC revenue summary",
            // `pit-` is an English fragment; the UUID anchor is what keeps
            // this readable rather than a permissive `pit-[A-Za-z0-9-]{20,}`.
            "pit-stop-strategy-analysis for the race",
            // Bare prefixes with nothing key-shaped after them.
            "the AKIA meeting notes",
            "EAA is the airport code",
            // Colon-separated pairs that are not Telegram tokens.
            "timestamp 1234567:30 remaining",
            "map 127.0.0.1:8080 to the proxy",
        ] {
            assert_eq!(
                s.scrub(text),
                text,
                "ordinary text must survive the strip verbatim: {text}"
            );
        }
    }

    /// A real key is still caught at its published length, and inside
    /// punctuation, so anchoring the format did not open a hole.
    #[test]
    fn still_scrubs_aws_keys_at_their_published_length() {
        let s = s();
        for text in [
            "AKIAFAKEFAKEFAKEFAKE",
            "aws_access_key_id=ASIAFAKEFAKEFAKEFAKE",
            "(AKIAFAKEFAKEFAKEFAKE)",
            "\"ASIAFAKEFAKEFAKEFAKE\",",
        ] {
            let out = s.scrub(text);
            assert!(out.contains("[REDACTED:"), "not redacted: {text}");
            assert!(!out.contains("FAKEFAKE"), "leaked: {text}");
        }
    }

    #[test]
    fn scrubs_aws_temporary_session_key_id() {
        // ASIA… is an STS short-lived key id; AKIA… was already covered.
        let out = s().scrub("aws_access_key_id ASIAFAKEFAKEFAKEFAKE");
        assert!(out.contains("[REDACTED:"));
        assert!(!out.contains("ASIAFAKE"));
    }

    #[test]
    fn scrubs_stripe_restricted_key() {
        let out = s().scrub("stripe=rk_live_FAKEfakeFAKE1234");
        assert!(out.contains("[REDACTED:"));
        assert!(!out.contains("FAKEfakeFAKE"));
    }

    #[test]
    fn scrubs_meta_graph_access_token() {
        let out = s().scrub("fb=EAAFAKEfakeFAKEfake0123456789");
        assert!(out.contains("[REDACTED:"));
        assert!(!out.contains("FAKEfake"));
    }

    #[test]
    fn meta_pattern_does_not_eat_short_base64_runs() {
        // Negative control. The OpenSSH fixture in
        // `scrubs_pem_private_key_block` contains the substring "EAAAAA";
        // the {20,} tail is what stops `EAA…` from matching every base64
        // blob that happens to contain it.
        let out = s().scrub("harmless b3BlbnNzaC1rZXktdjEAAAAA value");
        assert!(!out.contains("[REDACTED:"));
    }

    #[test]
    fn scrubs_google_oauth_refresh_token() {
        let out = s().scrub("refresh_token: 1//0gFAKEfakeFAKEfake0123456789");
        assert!(out.contains("[REDACTED:"));
        assert!(!out.contains("FAKEfake"));
    }

    #[test]
    fn scrubs_telegram_bot_token() {
        let out = s().scrub("TG 123456789:AAFAKEfakeFAKEfakeFAKEfake0123456789 done");
        assert!(out.contains("[REDACTED:"));
        assert!(!out.contains("FAKEfake"));
        assert!(out.contains("done"), "should not swallow trailing context");
    }

    #[test]
    fn scrubs_telegram_documented_example_shape() {
        // Regression for #408: Telegram's own Bot API docs publish
        // `123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11` — a 6-digit id and no
        // `AA` prefix — which the original `\d{8,10}:AA…` form could not match.
        // This is the vendor's placeholder, not a live token.
        let out = s().scrub("token 123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11 ok");
        assert!(out.contains("[REDACTED:"));
        assert!(!out.contains("ABC-DEF1234"));
        assert!(out.contains("ok"), "should not swallow trailing context");
    }

    #[test]
    fn telegram_pattern_does_not_eat_colon_separated_prose() {
        // Negative control, and the reason the non-`AA` branch is
        // length-anchored rather than open-ended: dropping the anchor entirely
        // makes `\d+:[\w-]+` match all of these.
        for benign in [
            "built 2026:08 release notes",
            "aspect ratio 16:9 widescreen",
            "ports 8080:my-service-name-here",
            "00:00:00,000 --> 00:00:04,120 caption",
            "commit 12345678:deadbeefcafebabe0123456789abcdef",
        ] {
            let out = s().scrub(benign);
            assert!(!out.contains("[REDACTED:"), "false positive on: {benign}");
        }
    }

    #[test]
    fn scrubs_gohighlevel_private_integration_token() {
        // Uppercase hex on purpose: the vendor documents no case, so the
        // pattern accepts both. DEADBEEF/CAFEBABE keeps the fixture
        // unmistakably synthetic.
        let out = s().scrub("ghl=pit-DEADBEEF-FACE-4B0B-BEEF-CAFEBABE1234");
        assert!(out.contains("[REDACTED:"));
        assert!(!out.contains("DEADBEEF"));
    }

    #[test]
    fn ghl_pattern_does_not_eat_hyphenated_english() {
        // Negative control, and the reason the tail is UUID-anchored rather
        // than permissive: `pit-` is an ordinary English fragment.
        let out = s().scrub("planning the pit-stop-strategy-analysis for turn 4");
        assert!(!out.contains("[REDACTED:"));
    }

    #[test]
    fn scrubs_slack_bot_token() {
        let out = s().scrub("slack=xoxb-1234567890-abcdefghij");
        assert!(out.contains("[REDACTED:"));
        assert!(!out.contains("xoxb-1234"));
    }

    #[test]
    fn scrubs_pem_private_key_block() {
        let pem = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\nafter";
        let out = s().scrub(pem);
        assert!(out.contains("[REDACTED:"));
        assert!(!out.contains("b3BlbnNz"));
        assert!(out.contains("before"));
        assert!(out.contains("after"));
    }

    #[test]
    fn scrubs_url_embedded_credentials() {
        let out = s().scrub("connect to postgres://admin:hunter2@db.internal/prod");
        assert!(out.contains("[REDACTED:"));
        assert!(!out.contains("hunter2"));
    }

    #[test]
    fn scrubs_generic_env_var_assignments() {
        let out = s().scrub("MY_INTERNAL_API_KEY=aaaaaaaaaaaa");
        assert!(out.contains("[REDACTED:"));
        let out2 = s().scrub("SOMETHING_SECRET=foo");
        assert!(out2.contains("[REDACTED:"));
        let out3 = s().scrub("DB_PASSWORD=hunter2");
        assert!(out3.contains("[REDACTED:"));
    }

    /// The value character class excludes `[`, so a value already replaced
    /// with `[REDACTED]` by an earlier pattern cannot be matched again and
    /// lose its header name.
    #[test]
    fn auth_header_pattern_does_not_re_eat_an_earlier_redaction() {
        let out = s().scrub("X-Api-Key: Bearer FAKEfakeFAKEfake0123456789");
        assert_eq!(out, "X-Api-Key: [REDACTED:bearer_token]");
    }

    /// The header name is matched with a flat character class rather than
    /// nested quantifiers, and `regex` is backtracking-free. This pins linear
    /// behaviour on an adversarial hyphen run instead of asserting on a wall
    /// clock, which would be flaky in CI.
    #[test]
    fn auth_header_pattern_handles_adversarial_hyphen_runs() {
        let long_name = format!("X{}", "-a".repeat(4_000));
        let secret = "F".repeat(40);

        let matching = format!("{long_name}-auth-token: {secret}");
        let out = s().scrub(&matching);
        assert!(out.contains("[REDACTED:"), "adversarial match not redacted");
        assert!(!out.contains(&secret), "leaked under adversarial input");

        let non_matching = format!("{long_name}-harmless: {secret}");
        assert_eq!(s().scrub(&non_matching), non_matching);
    }

    /// `-Key` and `-Token` suffixes are also used by non-secret headers:
    /// idempotency keys, pagination cursors, storage partition keys. Those
    /// carry no credential and stay useful when reading captured tool
    /// output, so the rule requires an auth qualifier before the suffix.
    #[test]
    fn auth_header_pattern_leaves_non_secret_key_and_token_headers_alone() {
        // Values use the low-entropy "obvious fake" shape the sanitize
        // fixtures follow (see .gitleaks.toml) so the secret scanner does
        // not flag them; the assertion is name-based, so the value shape
        // is irrelevant to what is being tested.
        for text in [
            "Idempotency-Key: 00000000-0000-0000-0000-000000000000",
            "Continuation-Token: 0000000000000000000000",
            "Next-Page-Token: 0000000000000000000000000000",
            "Partition-Key: user-000000000000001",
            "Cache-Key: v2-catalog-000000000000",
        ] {
            assert_eq!(s().scrub(text), text, "over-redacted: {text}");
        }
    }

    #[test]
    fn scrubs_cloud_credential_paths() {
        let out = s().scrub("read /home/user/.aws/credentials");
        assert!(out.contains("[REDACTED:"));
        let out2 = s().scrub("set KUBECONFIG=/home/user/.kube/config");
        assert!(out2.contains("[REDACTED:"));
    }

    /// Windows agent output uses backslashes. The Unix-only `/.ssh` rules left
    /// `C:\Users\alice\.ssh\id_rsa` (and the same for `.aws` / `.kube` /
    /// `.gnupg` / `.config\gcloud`) in the stored observation.
    #[test]
    fn scrubs_windows_credential_paths() {
        for text in [
            r"read C:\Users\alice\.ssh\id_rsa",
            r"copy C:\Users\alice\.aws\credentials",
            r"KUBECONFIG=C:\Users\alice\.kube\config",
            r"GNUPGHOME=C:\Users\alice\.gnupg\private-keys-v1.d",
            r"gcloud auth C:\Users\alice\.config\gcloud\credentials.db",
        ] {
            let out = s().scrub(text);
            assert!(out.contains("[REDACTED:"), "not redacted: {text} -> {out}");
            assert!(
                !out.to_ascii_lowercase().contains(r"\users\alice\"),
                "leaked profile path: {text} -> {out}"
            );
        }
        // Unix paths still redact after the separator class is widened.
        let unix = s().scrub("see /home/user/.ssh/id_ed25519");
        assert!(unix.contains("[REDACTED:"));
        assert!(!unix.contains("/home/user/.ssh"));
    }

    /// Opaque auth headers reach capture via tool output echoing curl. The
    /// `bearer\s+` rule needs the literal keyword and the generic env rule
    /// needs `UPPER_SNAKE_TOKEN=`, so a kebab-case header matched neither.
    #[test]
    fn scrubs_opaque_auth_headers_without_bearer_keyword() {
        for header in [
            // AWS SigV4 session credential: a real, widely-emitted header
            // that carries a secret with no scheme keyword.
            "X-Amz-Security-Token: FAKEfakeFAKEfake0123456789",
            // GitLab.
            "Private-Token: FAKEfakeFAKEfake0123456789",
            "X-Api-Key: FAKEfakeFAKEfake0123456789",
            "Api-Key: FAKEfakeFAKEfake0123456789",
            "X-Auth-Token: FAKEfakeFAKEfake0123456789",
            // Azure API Management, Google, RapidAPI: the auth qualifier is
            // not always its own segment.
            "Ocp-Apim-Subscription-Key: FAKEfakeFAKEfake0123456789",
            "X-Goog-Api-Key: FAKEfakeFAKEfake0123456789",
            "X-RapidAPI-Key: FAKEfakeFAKEfake0123456789",
            // A vendor-specific auth header: bare hex, no scheme keyword.
            "Acme-Authentication: FAKEfake0123456789abcdef0123456789abcdef0123456789abcdef01234567",
            // Header casing is not normalised by the emitting tool.
            "x-api-key: FAKEfakeFAKEfake0123456789",
        ] {
            let out = s().scrub(header);
            assert!(out.contains("[REDACTED:"), "not redacted: {header}");
            assert!(!out.contains("FAKEfake"), "leaked: {header}");
        }
    }

    /// The rule keys off an auth-ish word in the header *name*, so ordinary
    /// hyphenated headers with long values must survive byte-identical.
    #[test]
    fn auth_header_pattern_does_not_eat_ordinary_headers() {
        for text in [
            "Content-Type: application/json;charset=utf-8",
            "Accept-Language: en-US,en;q=0.9",
            "User-Agent: ExampleApp/3.1 ExampleOS/27.0 build/24A431",
            "Cache-Control: max-age=0, s-maxage=0, no-cache, no-store",
            "X-Request-Context: 000000-1,29 t:example99",
            "Content-Length: 4911",
        ] {
            let out = s().scrub(text);
            assert_eq!(out, text, "over-redacted: {text}");
        }
    }

    #[test]
    fn observation_round_trip() {
        let raw = NewObservation {
            occurred_at: None,
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            project_id: ProjectId::new(),
            kind: ObservationKind::UserPrompt,
            extension: None,
            source_event: None,
            title: "OPENAI_API_KEY=sk-leak-1234567890abcdef".into(),
            body: "see /home/user/.ssh/id_ed25519".into(),
            importance: 5,
        };
        let scrubbed = Sanitized::new(raw, &s()).into_inner();
        assert!(scrubbed.title.contains("[REDACTED:"));
        assert!(scrubbed.body.contains("[REDACTED:"));
    }

    #[test]
    fn observation_boundary_caps_after_sanitizing_without_splitting_utf8() {
        let raw = NewObservation {
            occurred_at: None,
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            project_id: ProjectId::new(),
            kind: ObservationKind::UserPrompt,
            extension: None,
            source_event: None,
            title: "large prompt".into(),
            body: format!(
                "OPENAI_API_KEY=sk-{} {} TAIL_SENTINEL",
                "a".repeat(40),
                "é".repeat(OBSERVATION_BODY_MAX_BYTES)
            ),
            importance: 8,
        };
        let scrubbed = Sanitized::new(raw, &s()).into_inner();
        assert!(scrubbed.body.len() <= OBSERVATION_BODY_MAX_BYTES);
        assert!(scrubbed.body.contains("[REDACTED:"));
        // Head-tail truncation preserves the tail sentinel so the LLM
        // consolidator sees both the start and the end of the original body.
        assert!(scrubbed.body.contains("TAIL_SENTINEL"));
        assert!(scrubbed.body.contains("[truncated"));
    }

    /// Regression for #980: a secret matched only by an operator's
    /// `[sanitize] extra_patterns` rule must be redacted in the title even
    /// when the raw title is long enough to need the 80-char cap — proving
    /// `Sanitized::new` scrubs `title` before truncating it, the same order
    /// the body already used. See `issue_980_user_prompt_title_is_sanitized_
    /// before_truncated` in `ai-memory-hooks::router` for the full pipeline
    /// (payload extraction -> router -> here) this guards end to end.
    #[test]
    fn title_is_scrubbed_before_the_80_char_cap_runs() {
        let sanitizer = Sanitizer::new(&SanitizeConfig {
            extra_patterns: vec![r"CANARY-[0-9]{20,}".to_string()],
            allowlist: Vec::new(),
        })
        .unwrap();
        let secret = format!("CANARY-{}", "9".repeat(25));
        let raw = NewObservation {
            occurred_at: None,
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            project_id: ProjectId::new(),
            kind: ObservationKind::UserPrompt,
            extension: None,
            source_event: None,
            title: format!("{} {secret}", "x".repeat(70)),
            body: String::new(),
            importance: 5,
        };
        let scrubbed = Sanitized::new(raw, &sanitizer).into_inner();
        assert!(
            !scrubbed.title.contains(&secret),
            "raw secret survived scrub+cap: {:?}",
            scrubbed.title
        );
        assert!(
            scrubbed.title.contains("REDACT"),
            "title lost all trace of redaction: {:?}",
            scrubbed.title
        );
        assert!(scrubbed.title.chars().count() <= 80);
    }

    #[test]
    fn utf8_truncation_reserves_the_ellipsis_inside_the_cap() {
        let truncated = truncate_utf8_bytes("abcééé", 7);
        assert_eq!(truncated, "abc…");
        assert_eq!(truncated.len(), 6);
        assert_eq!(truncate_utf8_bytes("unchanged", 9), "unchanged");
        assert!(truncate_utf8_bytes("large", 2).is_empty());
    }

    /// Moved from `ai-memory-hooks::payload` (see #980): the char-count cap
    /// and ellipsis behaviour are unchanged from the original
    /// `payload::truncate_for_title`, only *where* and *when* it runs moved.
    #[test]
    fn title_truncation_caps_at_80_chars_with_ellipsis() {
        let short = "a short title";
        assert_eq!(truncate_for_title(short), short);

        let exactly_80 = "x".repeat(80);
        assert_eq!(truncate_for_title(&exactly_80), exactly_80);

        let long = "x".repeat(200);
        let truncated = truncate_for_title(&long);
        assert_eq!(truncated.chars().count(), 80);
        assert!(truncated.ends_with('…'));
        assert_eq!(&truncated[..79], &"x".repeat(79));
    }

    /// Char-count truncation must never split a multi-byte code point, even
    /// though it counts *characters*, not bytes, unlike [`truncate_utf8_bytes`].
    #[test]
    fn title_truncation_is_utf8_safe_on_multibyte_input() {
        // 100 é's (each 2 bytes in UTF-8): a byte-oriented cap at 80 would
        // risk landing mid-codepoint; a char-oriented cap never can.
        let input = "é".repeat(100);
        let truncated = truncate_for_title(&input);
        assert_eq!(truncated.chars().count(), 80);
        assert!(truncated.ends_with('…'));
        // Every remaining char before the ellipsis is a complete `é`.
        assert!(
            truncated[..truncated.len() - '…'.len_utf8()]
                .chars()
                .all(|c| c == 'é')
        );
        // The string is valid UTF-8 by construction (it's a `String`), but
        // assert the byte length is exactly what 79 `é`s + one `…` costs,
        // as a belt-and-suspenders check that nothing was sliced mid-byte.
        assert_eq!(truncated.len(), 79 * 'é'.len_utf8() + '…'.len_utf8());
    }

    #[test]
    fn head_tail_truncation_preserves_head_and_tail() {
        let input = format!("HEAD{}TAIL", "x".repeat(200));
        let truncated = truncate_utf8_bytes_head_tail(&input, 128);
        assert!(truncated.starts_with("HEAD"));
        assert!(truncated.ends_with("TAIL"));
        assert!(truncated.contains("[truncated"));
        assert!(truncated.len() <= 128);
    }

    #[test]
    fn head_tail_truncation_short_input_unchanged() {
        let input = "short body";
        assert_eq!(truncate_utf8_bytes_head_tail(input, 128), "short body");
    }

    #[test]
    fn head_tail_truncation_small_budget_falls_back_to_head_only() {
        let input = "HEADxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxTAIL";
        let truncated = truncate_utf8_bytes_head_tail(input, 32);
        assert!(truncated.ends_with('…'));
        assert!(!truncated.contains("TAIL"));
    }

    #[test]
    fn head_tail_truncation_no_split_on_utf8_boundary() {
        let input = format!("H{}T", "é".repeat(200));
        let truncated = truncate_utf8_bytes_head_tail(&input, 128);
        // Must not split a code point — the tail should start at a char
        // boundary and the result must be valid UTF-8.
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
        assert!(truncated.ends_with('T'));
    }

    #[test]
    fn extra_patterns_compile_and_apply() {
        let cfg = SanitizeConfig {
            extra_patterns: vec![r"CANARY-[0-9]+".to_string()],
            allowlist: vec![],
        };
        let sn = Sanitizer::new(&cfg).unwrap();
        let out = sn.scrub("found CANARY-42 here");
        assert!(out.contains("[REDACTED:"));
    }

    /// Typed labels let a later reader distinguish secret kinds without
    /// reconstructing them from context — the point of P1.
    #[test]
    fn typed_labels_identify_the_secret_kind() {
        let out = s().scrub("token=ghp_FAKEfakeFAKEfakeFAKEfake012345678");
        assert!(out.contains("[REDACTED:github_token]"), "got: {out}");

        let out = s().scrub("jwt=eyJFAKEfakeFAKEfake.eyJFAKEfakeFAKEfake.FAKEfakeFAKEfakeFAKEfake");
        assert!(out.contains("[REDACTED:jwt]"), "got: {out}");

        let out = s().scrub("OPENAI_API_KEY=sk-leak-1234567890abcdef");
        assert!(out.contains("[REDACTED:env_secret]"), "got: {out}");
    }

    #[test]
    fn extra_pattern_match_is_labeled_custom() {
        let cfg = SanitizeConfig {
            extra_patterns: vec![r"CANARY-[0-9]+".to_string()],
            allowlist: vec![],
        };
        let sn = Sanitizer::new(&cfg).unwrap();
        let out = sn.scrub("found CANARY-42 here");
        assert_eq!(out, "found [REDACTED:custom] here");
    }

    /// The value character class excludes `[`, so an already-labeled marker
    /// cannot be swallowed by a later pattern and lose its kind.
    #[test]
    fn double_scrub_is_idempotent() {
        let once = s().scrub("token=ghp_FAKEfakeFAKEfakeFAKEfake012345678");
        assert!(once.contains("[REDACTED:github_token]"));
        let twice = s().scrub(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn invalid_extra_pattern_errors() {
        let cfg = SanitizeConfig {
            extra_patterns: vec!["[unterminated".to_string()],
            allowlist: vec![],
        };
        assert!(Sanitizer::new(&cfg).is_err());
    }

    #[test]
    fn allowlist_substring_survives_redaction() {
        // The generic env-var pattern would match this, but the
        // allowlist exempts spans containing "PROJECT_TOKEN".
        let cfg = SanitizeConfig {
            extra_patterns: vec![],
            allowlist: vec!["PROJECT_TOKEN_PUBLIC".to_string()],
        };
        let sn = Sanitizer::new(&cfg).unwrap();
        let out = sn.scrub("we use PROJECT_TOKEN_PUBLIC=abc internally");
        assert!(
            out.contains("PROJECT_TOKEN_PUBLIC"),
            "allowlist span should survive; got: {out}"
        );
    }

    /// One line of a fake key body. Short on purpose: a complete fake block
    /// stays under the length the secret scanner's private-key rule needs.
    const FAKE_KEY_LINE: &str = "b3BlbnNzaC1rZXktdjEAAAAA";

    /// The vectors the client companion's scrub runs too
    /// (`companions/ai-memory-client`), so the two copies cannot drift.
    #[test]
    fn shared_sanitizer_vectors_hold() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/sanitizer_vectors.json"
        ))
        .unwrap();
        let strings = |key: &str| -> Vec<String> {
            vectors[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap().to_string())
                .collect()
        };
        for secret in strings("credentials") {
            let (head, tail) = secret.split_at(secret.len() / 2);
            for invisible in strings("invisibles") {
                let out = s().scrub(&format!("key {head}{invisible}{tail} end"));
                assert!(
                    out.contains("[REDACTED:") && !out.contains(head) && !out.contains(tail),
                    "{invisible:?} inside {secret}: {out:?}"
                );
                assert!(out.starts_with("key ") && out.ends_with(" end"), "{out:?}");
            }
        }
        let body_line = vectors["key_body_line"].as_str().unwrap();
        for block in vectors["key_blocks"].as_array().unwrap() {
            let input = block["input"].as_str().unwrap();
            let out = s().scrub(input);
            assert!(
                out.contains("[REDACTED:private_key]") && !out.contains(body_line),
                "{input:?} -> {out:?}"
            );
            for kept in block["kept"].as_array().unwrap() {
                assert!(out.contains(kept.as_str().unwrap()), "{input:?} -> {out:?}");
            }
        }
        for text in strings("unchanged") {
            assert_eq!(s().scrub(&text), text);
        }
    }

    #[test]
    fn invisible_character_inside_an_operator_pattern_match_is_redacted() {
        let sn = Sanitizer::new(&SanitizeConfig {
            extra_patterns: vec![r"CANARY-[0-9]{6}".to_string()],
            allowlist: vec![],
        })
        .unwrap();
        assert_eq!(
            sn.scrub("found CANARY-12\u{200b}3456 here"),
            "found [REDACTED:custom] here"
        );
    }

    /// An invisible character neither breaks an allowlist entry (the span is
    /// still exempt) nor lets a secret that is not allowlisted through.
    #[test]
    fn invisible_characters_neither_break_nor_bypass_the_allowlist() {
        let sn = Sanitizer::new(&SanitizeConfig {
            extra_patterns: vec![],
            allowlist: vec!["KEEP_ME".to_string()],
        })
        .unwrap();
        for kept in [
            "SERVICE_TOKEN=KEEP_ME_visible_value",
            "SERVICE_TOKEN=KEEP\u{200b}_ME_visible_value",
        ] {
            assert_eq!(sn.scrub(kept), kept, "allowlisted span must survive");
        }
        let out = sn.scrub("SERVICE_TOKEN=s3cretva\u{200b}lue0123");
        assert!(
            out.contains("[REDACTED:env_secret]") && !out.contains("lue0123"),
            "a secret without the allowlist entry must still be redacted: {out:?}"
        );
    }

    #[test]
    fn characters_that_hide_text_are_stripped() {
        let hidden: String = "hidden note"
            .chars()
            .map(|c| char::from_u32(0xe0000 + u32::from(c)).unwrap())
            .collect();
        let out = s().scrub(&format!("visible{hidden} text"));
        assert_eq!(out, "visible text");

        assert_eq!(s().scrub("a\u{2062}b\u{feff}c"), "abc");
        // A flag start with no cancel tag is not a flag: its tags go.
        assert_eq!(s().scrub("\u{1f3f4}\u{e0067}\u{e0062} x"), "\u{1f3f4} x");
    }

    #[test]
    fn pgp_private_key_block_is_redacted() {
        // Split so secret scanners don't flag this test fixture as a real PGP key block.
        let begin = concat!("-----BEGIN PGP ", "PRIVATE KEY BLOCK-----");
        let end = concat!("-----END PGP ", "PRIVATE KEY BLOCK-----");
        let block = format!("before\n{begin}\nVersion: 1\n\n{FAKE_KEY_LINE}\n{end}\nafter");
        assert_eq!(s().scrub(&block), "before\n[REDACTED:private_key]\nafter");
    }

    #[test]
    fn private_key_block_without_an_end_line_is_redacted() {
        let line = FAKE_KEY_LINE;
        for (input, expected) in [
            (
                format!("$ head -3 key.pem\n-----BEGIN PRIVATE KEY-----\n{line}\n{line}\n$ ls"),
                "$ head -3 key.pem\n[REDACTED:private_key]\n$ ls".to_string(),
            ),
            (
                format!("-----BEGIN RSA PRIVATE KEY-----\r\n{line}\r\n{line}\r\n"),
                "[REDACTED:private_key]\r\n".to_string(),
            ),
            (
                format!(r#"{{"stdout":"-----BEGIN OPENSSH PRIVATE KEY-----\n{line}\n{line}"}}"#),
                r#"{"stdout":"[REDACTED:private_key]"}"#.to_string(),
            ),
            (
                format!(
                    "  -----BEGIN EC PRIVATE KEY-----\n  {line}\n  {}",
                    &line[..20]
                ),
                "  [REDACTED:private_key]".to_string(),
            ),
            (
                format!(
                    "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\n\
                     DEK-Info: AES-128-CBC,0123456789ABCDEF\n\n{line}\n"
                ),
                "[REDACTED:private_key]\n".to_string(),
            ),
        ] {
            assert_eq!(s().scrub(&input), expected, "input: {input:?}");
        }
    }

    /// A key body line can contain `eyJ…`, which a token rule would redact
    /// in part, leaving the rest of the key unrecognizable to the key rules.
    #[test]
    fn key_rules_run_before_token_rules_can_split_a_key_body() {
        let first = format!("MIIEeyJ{}", "A".repeat(50));
        let input = format!("-----BEGIN PRIVATE KEY-----\n{first}\n{FAKE_KEY_LINE}\n");
        assert_eq!(s().scrub(&input), "[REDACTED:private_key]\n");
    }

    #[test]
    fn allowlist_never_exempts_a_private_key() {
        let sn = Sanitizer::new(&SanitizeConfig {
            extra_patterns: vec![],
            allowlist: vec!["b3Bl".to_string()],
        })
        .unwrap();
        let complete =
            format!("-----BEGIN PRIVATE KEY-----\n{FAKE_KEY_LINE}\n-----END PRIVATE KEY-----");
        assert_eq!(sn.scrub(&complete), "[REDACTED:private_key]");
        let unterminated = format!("-----BEGIN PRIVATE KEY-----\n{FAKE_KEY_LINE}\n");
        assert_eq!(sn.scrub(&unterminated), "[REDACTED:private_key]\n");
    }

    #[test]
    fn blank_allowlist_entries_are_ignored() {
        let input = "token=ghp_FAKEfakeFAKEfakeFAKEfake012345678";
        let sn = Sanitizer::new(&SanitizeConfig {
            extra_patterns: vec![],
            allowlist: vec![String::new(), "   ".to_string()],
        })
        .unwrap();
        assert_eq!(sn.scrub(input), "token=[REDACTED:github_token]");

        // Control: a real entry still exempts its span.
        let sn = Sanitizer::new(&SanitizeConfig {
            extra_patterns: vec![],
            allowlist: vec!["ghp_FAKE".to_string()],
        })
        .unwrap();
        assert_eq!(sn.scrub(input), input);
    }

    /// JSON is the shape most captured tool payloads arrive in, and the
    /// quote before the value used to put it outside the value class, so the
    /// same secret was redacted in YAML and stored verbatim in JSON.
    #[test]
    fn scrubs_secret_values_in_json() {
        for raw in [
            r#"{"db_password":"correct-horse-battery"}"#,
            r#"{"password": "hunter2hunter2"}"#,
            r#""api_key": "FAKEfake0123456789abcdef""#,
            r#"{"client-token":"FAKEfake0123456789"}"#,
        ] {
            let out = s().scrub(raw);
            assert!(
                out.contains("[REDACTED:"),
                "JSON-quoted secret must be redacted: {raw} -> {out}"
            );
        }
    }

    /// `Authorization: Basic <base64>` carries `user:password`. The scheme
    /// word sits where the value class expected the secret, so the rule that
    /// names `authorization` never fired; only `Bearer` was caught, and only
    /// because a separate pattern matches the scheme keyword itself.
    #[test]
    fn scrubs_scheme_prefixed_authorization_headers() {
        for raw in [
            "Authorization: Basic YWRtaW46c2VjcmV0cGFzc3dvcmQ=",
            "authorization: Token FAKEfake0123456789",
            "Proxy-Authorization: Digest cmVhbG09ZXhhbXBsZQ==",
        ] {
            let out = s().scrub(raw);
            assert!(
                out.contains("[REDACTED:"),
                "scheme-prefixed credential must be redacted: {raw} -> {out}"
            );
        }
    }

    /// Azure storage connection strings and npm `_authToken` name the secret
    /// without the underscore the generic env rule requires.
    #[test]
    fn scrubs_unprefixed_account_and_auth_token_assignments() {
        for raw in [
            "DefaultEndpointsProtocol=https;AccountName=x;AccountKey=FAKEfake0123456789==;",
            "//registry.npmjs.org/:_authToken=npm_FAKEfake0123456789abcdefghij",
        ] {
            let out = s().scrub(raw);
            assert!(
                out.contains("[REDACTED:"),
                "assignment must be redacted: {raw} -> {out}"
            );
        }
    }

    /// Page bodies and observations are replayed to a terminal by
    /// `ai-memory read-page` / `search`, and a NUL makes the markdown file
    /// binary, so `grep` skips it and git stops diffing it.
    #[test]
    fn strips_terminal_control_sequences_and_nul() {
        let out = s().scrub(
            "red \u{1b}[31mtext\u{1b}[0m title \u{1b}]0;pwned\u{7}\
             bidi \u{202e}reversed\u{202c} nul \u{0}end\n\ttabbed",
        );
        for bad in ['\u{1b}', '\u{7}', '\u{0}', '\u{202e}', '\u{202c}'] {
            assert!(
                !out.contains(bad),
                "{bad:?} must not survive into a page or a terminal: {out:?}"
            );
        }
        assert!(
            !out.contains("[31m") && !out.contains("]0;"),
            "the sequence bodies must go with their escapes: {out:?}"
        );
        assert!(
            out.contains("red text title ") && out.ends_with("end\n\ttabbed"),
            "text, newlines and tabs must survive: {out:?}"
        );
    }

    /// The value floor is what keeps ordinary `key: value` prose intact; the
    /// new quote and scheme allowances must not erode it.
    #[test]
    fn keeps_short_and_non_secret_values() {
        for raw in [
            "Access-Control-Allow-Credentials: true",
            "Idempotency-Key: abc",
            "note: the password is stored in 1Password",
        ] {
            let out = s().scrub(raw);
            assert_eq!(out, raw, "must be left alone: {raw}");
        }
    }
}
