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
//! github_pat_…, Google AIza… plus OAuth refresh tokens 1//…, Meta /
//! Facebook Graph EAA…, Telegram bot tokens, GoHighLevel pit-…, Slack
//! xoxb/xoxp…, AWS AKIA/ASIA…), PEM-bracketed private
//! keys, URL-embedded credentials (`postgres://user:pass@host`), and
//! anything matching the generic `*_(KEY|TOKEN|SECRET|PASSWORD|
//! CREDENTIAL)=value` shape. Operators can extend the list via
//! `[sanitize].extra_patterns` and exempt substrings via
//! `[sanitize].allowlist` — the allowlist is checked *per match*, so a
//! pattern still runs but an allowlisted span survives unchanged.
//!
//! ## What we deliberately do not catch
//!
//! Standalone high-entropy strings (e.g. a 32-char random hex) cannot
//! be safely redacted without knowing their structure — too many false
//! positives. Operators who care about that level of paranoia should
//! add a custom pattern via `extra_patterns`.

use std::sync::Arc;

use regex::Regex;
use tracing::debug;

use crate::NewObservation;

/// Universal durable-body ceiling for lifecycle observations. Event adapters
/// may impose smaller limits, but no sanitized observation can cross the store
/// boundary above 16 KiB.
pub const OBSERVATION_BODY_MAX_BYTES: usize = 16 * 1024;

/// Compile-time list of redaction patterns. Order is intentional:
/// more-specific patterns first. False positives are acceptable —
/// better to redact a stray hash than to leak a credential.
const BUILTIN_PATTERN_STRS: &[&str] = &[
    // Bearer-style tokens.
    r#"(?i)bearer\s+[A-Za-z0-9._\-+/=]{16,}"#,
    // Vendor-prefixed API keys.
    r"sk-[A-Za-z0-9_\-]{16,}",
    // Stripe secret *and* restricted keys. `rk_live_` is scoped rather than
    // full-access, but the scope is operator-chosen and routinely includes
    // charges/refunds — not meaningfully safer than `sk_live_`.
    r"(?:sk|rk)_live_[A-Za-z0-9_\-]{16,}",
    // Every GitHub token prefix, not only personal-access: `gho_` (OAuth —
    // what `gh auth login` stores on disk), `ghu_` (user-to-server),
    // `ghs_` (server-to-server / Actions), `ghr_` (refresh).
    r"gh[pousr]_[A-Za-z0-9]{20,}",
    r"github_pat_[A-Za-z0-9_]{20,}",
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
    r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
    // Naked Google / Gemini API keys.
    r"AIza[A-Za-z0-9_\-]{30,}",
    // Google OAuth refresh tokens. Longer-lived than the AIza keys above:
    // they mint fresh access tokens until explicitly revoked, so a leaked
    // one outlives the session it came from.
    r"1//[0-9A-Za-z_\-]{20,}",
    // Meta / Facebook Graph API access tokens (ad accounts, pages,
    // business management).
    r"EAA[A-Za-z0-9]{20,}",
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
    r"\b\d{6,10}:(?:AA[A-Za-z0-9_\-]{30,}|[A-Za-z0-9_\-]{34,35})\b",
    // GoHighLevel Private Integration Tokens. The `pit-` prefix is what the
    // vendor documents (their MCP guide shows `Bearer pit-your-token`); the
    // tail is NOT documented anywhere, and every token observed in the wild
    // carries a UUID. Anchoring on the UUID shape rather than a permissive
    // tail is deliberate: `pit-` is also an English fragment, so
    // `pit-[A-Za-z0-9\-]{20,}` would redact "pit-stop-strategy-analysis".
    // These tokens do not expire until manually revoked.
    r"pit-[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
    // Slack tokens (bot/user/admin/app-level/refresh).
    r"xox[abprs]-[A-Za-z0-9\-]{10,}",
    r"xapp-[A-Za-z0-9\-]{10,}",
    // JWTs (three base64url segments separated by dots).
    r"eyJ[A-Za-z0-9_\-]{16,}\.[A-Za-z0-9_\-]{16,}\.[A-Za-z0-9_\-]{16,}",
    // PEM private key blocks — multi-line, lazy match.
    r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
    // URL-embedded credentials: scheme://user:pass@host.
    r"[a-zA-Z][a-zA-Z0-9+\-.]*://[^:/\s]+:[^@\s]+@[^\s]+",
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
    r#"(?i)\b[A-Za-z0-9-]*(?:authentication|authorization|credentials?|password|passwd|apikey|[a-z0-9]*(?:api|auth|access|secret|security|private|session|refresh|client|consumer|subscription|app|bearer)-(?:key|token))\s*:\s*[A-Za-z0-9._~+/=-]{8,}"#,
    // Provider-specific env-var assignments (kept explicit for clarity
    // and so that bare `OPENAI_API_KEY=anything-at-all` still triggers
    // even without `sk-` shape).
    r#"(?i)(ANTHROPIC_API_KEY|OPENAI_API_KEY|OPENROUTER_API_KEY|VOYAGE_API_KEY|MISTRAL_API_KEY|GROQ_API_KEY|HF_TOKEN|HUGGINGFACE_TOKEN|AWS_(SECRET_)?ACCESS_KEY[A-Z_]*|GITHUB_TOKEN|GH_TOKEN|GITLAB_TOKEN|GOOGLE_API_KEY|GEMINI_API_KEY|OLLAMA_API_KEY)\s*[=:]\s*\S+"#,
    // Generic env-var catch-all: any *_KEY / *_TOKEN / *_SECRET /
    // *_PASSWORD / *_CREDENTIAL[S] / *_PRIVATE_KEY assignment.
    r#"(?i)\b[A-Z][A-Z0-9_]*_(KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|CREDENTIALS|PRIVATE_KEY)\s*[=:]\s*\S+"#,
    // Filesystem paths that commonly contain credentials.
    r"(?:/[^/\s]+)*/\.ssh(?:/[^\s]+)?",
    r"(?:/[^/\s]+)*/\.aws(?:/[^\s]+)?",
    r"(?:/[^/\s]+)*/\.kube(?:/[^\s]+)?",
    r"(?:/[^/\s]+)*/\.config/gcloud(?:/[^\s]+)?",
    r"(?:/[^/\s]+)*/\.gnupg(?:/[^\s]+)?",
];

/// Stateful sanitizer. Cheap to clone — wraps an `Arc` of compiled
/// patterns. Construct once at startup, then pass everywhere by clone.
#[derive(Clone)]
pub struct Sanitizer {
    inner: Arc<SanitizerInner>,
}

struct SanitizerInner {
    patterns: Vec<Regex>,
    allowlist: Vec<String>,
}

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
        let mut patterns =
            Vec::with_capacity(BUILTIN_PATTERN_STRS.len() + cfg.extra_patterns.len());
        for p in BUILTIN_PATTERN_STRS {
            patterns.push(Regex::new(p)?);
        }
        for p in &cfg.extra_patterns {
            patterns.push(Regex::new(p)?);
        }
        Ok(Self {
            inner: Arc::new(SanitizerInner {
                patterns,
                allowlist: cfg.allowlist.clone(),
            }),
        })
    }

    /// Built-in-only sanitizer (no operator extras, no allowlist).
    /// Convenient for tests and zero-config callers.
    #[must_use]
    pub fn builtin() -> Self {
        Self::new(&SanitizeConfig::default()).expect("built-in patterns compile")
    }

    /// Scrub a single string. Each match is replaced with `[REDACTED]`
    /// unless the matched substring contains an allowlist entry, in
    /// which case it is left alone.
    #[must_use]
    pub fn scrub(&self, input: &str) -> String {
        let mut out = input.to_string();
        for re in &self.inner.patterns {
            out = re
                .replace_all(&out, |caps: &regex::Captures<'_>| {
                    let m = caps.get(0).map(|m| m.as_str()).unwrap_or_default();
                    if self.inner.allowlist.iter().any(|a| m.contains(a)) {
                        m.to_string()
                    } else {
                        debug!(pattern = re.as_str(), "sanitize: redacted match");
                        "[REDACTED]".to_string()
                    }
                })
                .into_owned();
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
    /// the universal durable-body ceiling after redaction.
    #[must_use]
    pub fn new(mut obs: NewObservation, sanitizer: &Sanitizer) -> Self {
        obs.title = sanitizer.scrub(&obs.title);
        obs.body =
            truncate_utf8_bytes_head_tail(&sanitizer.scrub(&obs.body), OBSERVATION_BODY_MAX_BYTES);
        Self(obs)
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
    fn scrubs_bearer_token() {
        let out = s().scrub("Authorization: Bearer abcdef0123456789ABCDEF0123456789");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("abcdef0123"));
    }

    #[test]
    fn scrubs_openrouter_key_via_sk_prefix() {
        let out = s().scrub("key=sk-or-v1-deadbeefcafebabe1234567890abcdef");
        assert!(out.contains("[REDACTED]"));
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
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("AIzaSy"));
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
            assert!(out.contains("[REDACTED]"), "not redacted: {tok}");
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
            assert!(out.contains("[REDACTED]"), "not redacted: {text}");
            assert!(!out.contains("FAKEFAKE"), "leaked: {text}");
        }
    }

    #[test]
    fn scrubs_aws_temporary_session_key_id() {
        // ASIA… is an STS short-lived key id; AKIA… was already covered.
        let out = s().scrub("aws_access_key_id ASIAFAKEFAKEFAKEFAKE");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("ASIAFAKE"));
    }

    #[test]
    fn scrubs_stripe_restricted_key() {
        let out = s().scrub("stripe=rk_live_FAKEfakeFAKE1234");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("FAKEfakeFAKE"));
    }

    #[test]
    fn scrubs_meta_graph_access_token() {
        let out = s().scrub("fb=EAAFAKEfakeFAKEfake0123456789");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("FAKEfake"));
    }

    #[test]
    fn meta_pattern_does_not_eat_short_base64_runs() {
        // Negative control. The OpenSSH fixture in
        // `scrubs_pem_private_key_block` contains the substring "EAAAAA";
        // the {20,} tail is what stops `EAA…` from matching every base64
        // blob that happens to contain it.
        let out = s().scrub("harmless b3BlbnNzaC1rZXktdjEAAAAA value");
        assert!(!out.contains("[REDACTED]"));
    }

    #[test]
    fn scrubs_google_oauth_refresh_token() {
        let out = s().scrub("refresh_token: 1//0gFAKEfakeFAKEfake0123456789");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("FAKEfake"));
    }

    #[test]
    fn scrubs_telegram_bot_token() {
        let out = s().scrub("TG 123456789:AAFAKEfakeFAKEfakeFAKEfake0123456789 done");
        assert!(out.contains("[REDACTED]"));
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
        assert!(out.contains("[REDACTED]"));
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
            assert!(!out.contains("[REDACTED]"), "false positive on: {benign}");
        }
    }

    #[test]
    fn scrubs_gohighlevel_private_integration_token() {
        // Uppercase hex on purpose: the vendor documents no case, so the
        // pattern accepts both. DEADBEEF/CAFEBABE keeps the fixture
        // unmistakably synthetic.
        let out = s().scrub("ghl=pit-DEADBEEF-FACE-4B0B-BEEF-CAFEBABE1234");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("DEADBEEF"));
    }

    #[test]
    fn ghl_pattern_does_not_eat_hyphenated_english() {
        // Negative control, and the reason the tail is UUID-anchored rather
        // than permissive: `pit-` is an ordinary English fragment.
        let out = s().scrub("planning the pit-stop-strategy-analysis for turn 4");
        assert!(!out.contains("[REDACTED]"));
    }

    #[test]
    fn scrubs_slack_bot_token() {
        let out = s().scrub("slack=xoxb-1234567890-abcdefghij");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("xoxb-1234"));
    }

    #[test]
    fn scrubs_pem_private_key_block() {
        let pem = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\nafter";
        let out = s().scrub(pem);
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("b3BlbnNz"));
        assert!(out.contains("before"));
        assert!(out.contains("after"));
    }

    #[test]
    fn scrubs_url_embedded_credentials() {
        let out = s().scrub("connect to postgres://admin:hunter2@db.internal/prod");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("hunter2"));
    }

    #[test]
    fn scrubs_generic_env_var_assignments() {
        let out = s().scrub("MY_INTERNAL_API_KEY=aaaaaaaaaaaa");
        assert!(out.contains("[REDACTED]"));
        let out2 = s().scrub("SOMETHING_SECRET=foo");
        assert!(out2.contains("[REDACTED]"));
        let out3 = s().scrub("DB_PASSWORD=hunter2");
        assert!(out3.contains("[REDACTED]"));
    }

    /// The value character class excludes `[`, so a value already replaced
    /// with `[REDACTED]` by an earlier pattern cannot be matched again and
    /// lose its header name.
    #[test]
    fn auth_header_pattern_does_not_re_eat_an_earlier_redaction() {
        let out = s().scrub("X-Api-Key: Bearer FAKEfakeFAKEfake0123456789");
        assert_eq!(out, "X-Api-Key: [REDACTED]");
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
        assert!(out.contains("[REDACTED]"), "adversarial match not redacted");
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
        for text in [
            "Idempotency-Key: 3f7a1c2e-9b4d-4f88-a1e2-7c6b5d4e3f21",
            "Continuation-Token: 0000000000000000000000",
            "Next-Page-Token: CiAKGjBpNDd2Nmp2Zml2cWtwYjBk",
            "Partition-Key: user-000000000000001",
            "Cache-Key: v2-catalog-000000000000",
        ] {
            assert_eq!(s().scrub(text), text, "over-redacted: {text}");
        }
    }

    #[test]
    fn scrubs_cloud_credential_paths() {
        let out = s().scrub("read /home/user/.aws/credentials");
        assert!(out.contains("[REDACTED]"));
        let out2 = s().scrub("set KUBECONFIG=/home/user/.kube/config");
        assert!(out2.contains("[REDACTED]"));
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
            assert!(out.contains("[REDACTED]"), "not redacted: {header}");
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
        assert!(scrubbed.title.contains("[REDACTED]"));
        assert!(scrubbed.body.contains("[REDACTED]"));
    }

    #[test]
    fn observation_boundary_caps_after_sanitizing_without_splitting_utf8() {
        let raw = NewObservation {
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
        assert!(scrubbed.body.contains("[REDACTED]"));
        // Head-tail truncation preserves the tail sentinel so the LLM
        // consolidator sees both the start and the end of the original body.
        assert!(scrubbed.body.contains("TAIL_SENTINEL"));
        assert!(scrubbed.body.contains("[truncated"));
    }

    #[test]
    fn utf8_truncation_reserves_the_ellipsis_inside_the_cap() {
        let truncated = truncate_utf8_bytes("abcééé", 7);
        assert_eq!(truncated, "abc…");
        assert_eq!(truncated.len(), 6);
        assert_eq!(truncate_utf8_bytes("unchanged", 9), "unchanged");
        assert!(truncate_utf8_bytes("large", 2).is_empty());
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
        assert!(out.contains("[REDACTED]"));
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
}
