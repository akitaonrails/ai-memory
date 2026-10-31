//! Embedded Fluent catalogs shared by human-facing CLI and web views.
//!
//! A localizer is explicit, cheaply cloned and safe to share across async
//! tasks. Protocol data, user content and stored error codes stay untranslated.
//!
//! CLI errors keep their English source text, which tests and logs rely on.
//! [`Localizer::diagnostic`] translates them where they reach the terminal.

use std::{fmt, str::FromStr, sync::Arc};

use fluent_bundle::{FluentArgs, FluentResource, FluentValue, concurrent::FluentBundle};
use fluent_syntax::{ast, parser};
use serde::{Deserialize, Serialize};

const FIXED_ERRORS: &[(&str, &str)] = &[
    ("invalid credentials", "common-error-invalid-credentials"),
    (
        "authentication unavailable",
        "common-error-auth-unavailable",
    ),
    (
        "password confirmation does not match",
        "common-error-password-confirmation",
    ),
    ("password is reserved", "common-error-password-reserved"),
    (
        "invalid password: does not meet policy",
        "common-error-password-policy",
    ),
    ("csrf required", "common-error-csrf"),
    ("password change required", "common-error-password-change"),
    ("auth required", "common-error-auth-required"),
    ("too many login attempts", "common-error-login-rate"),
    (
        "Cross-origin form request refused",
        "common-error-cross-origin",
    ),
    (
        "Password change required",
        "common-error-management-password",
    ),
    ("Admin access required", "common-error-admin"),
    (
        "Use at least 12 characters.",
        "common-error-password-length",
    ),
    (
        "Both workspace and project are required",
        "common-error-scope-pair",
    ),
    ("Session ID required", "common-error-session-required"),
    (
        "Database deletion completed, but some wiki files could not be removed. Ask an Admin to check the server logs.",
        "common-error-files-remain",
    ),
];

/// Supported human interface languages, serialized as BCP 47 tags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    /// English is the compatibility fallback.
    #[default]
    #[serde(rename = "en", alias = "en-US", alias = "en-GB")]
    English,
    /// Brazilian Portuguese.
    #[serde(rename = "pt-BR", alias = "pt", alias = "pt_BR")]
    PortugueseBrazil,
}

impl Language {
    /// All compiled interface languages. Keep router-owned catalogs in sync with this list.
    pub const ALL: &[Self] = &[Self::English, Self::PortugueseBrazil];

    /// Canonical HTML/HTTP language tag.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::PortugueseBrazil => "pt-BR",
        }
    }

    /// Negotiate a bounded Accept-Language list, respecting preference weights.
    pub fn negotiate(header: &str) -> Self {
        let mut selected = Self::English;
        let mut best = 0.0_f32;
        if header.len() > 4096 {
            return selected;
        }
        for range in header.split(',').take(64) {
            let mut parts = range.trim().split(';');
            let name = parts.next().unwrap_or_default().trim();
            let weight = parts.try_fold(1.0_f32, |_, part| {
                let (key, value) = part.trim().split_once('=')?;
                if key != "q" {
                    return None;
                }
                value
                    .parse::<f32>()
                    .ok()
                    .filter(|q| (0.0..=1.0).contains(q))
            });
            let Some(weight) = weight.filter(|weight| *weight > best) else {
                continue;
            };
            let language = if name == "*" {
                Ok(Self::English)
            } else {
                name.parse()
            };
            if let Ok(language) = language {
                selected = language;
                best = weight;
            }
        }
        selected
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.tag())
    }
}

impl FromStr for Language {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value
            .trim()
            .split(['.', '@'])
            .next()
            .unwrap_or_default()
            .replace('_', "-")
            .to_ascii_lowercase();
        match normalized.as_str() {
            "en" | "en-us" | "en-gb" | "c" | "posix" => Ok(Self::English),
            "pt" | "pt-br" => Ok(Self::PortugueseBrazil),
            _ => Err("supported languages: en, pt-BR"),
        }
    }
}

/// Immutable catalogs owned by the caller, never process-global configuration.
#[derive(Clone)]
pub struct Localizer {
    language: Language,
    bundle: Arc<FluentBundle<FluentResource>>,
    fallback: Option<Arc<FluentBundle<FluentResource>>>,
    diagnostics: Arc<Vec<DiagnosticTemplate>>,
}

/// The English shape of one `diagnostics.ftl` message, used to recognize
/// already rendered text and recover its values.
#[derive(Debug)]
struct DiagnosticTemplate {
    id: String,
    /// One more literal than variables: `literals[i]` precedes `variables[i]`.
    literals: Vec<String>,
    variables: Vec<String>,
    literal_len: usize,
}

impl DiagnosticTemplate {
    /// Each value stops at the first occurrence of the literal after it.
    fn capture<'a>(&self, text: &'a str) -> Option<Vec<(&str, &'a str)>> {
        let mut rest = text.strip_prefix(self.literals[0].as_str())?;
        let mut values = Vec::with_capacity(self.variables.len());
        for (index, variable) in self.variables.iter().enumerate() {
            let literal = self.literals[index + 1].as_str();
            let end = if index + 1 == self.variables.len() {
                rest.len().checked_sub(literal.len())?
            } else {
                rest.find(literal)?
            };
            let (value, tail) = rest.split_at_checked(end)?;
            values.push((variable.as_str(), value));
            rest = tail.strip_prefix(literal)?;
        }
        rest.is_empty().then_some(values)
    }
}

/// Value names that hold a nested diagnostic rather than a path or identifier.
const NESTED_DIAGNOSTICS: &[&str] = &[
    "detail",
    "e",
    "err",
    "error",
    "failure",
    "reason",
    "rollback_err",
];

const MAX_DIAGNOSTIC_NESTING: usize = 4;

impl fmt::Debug for Localizer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Localizer")
            .field("language", &self.language)
            .finish_non_exhaustive()
    }
}

impl Default for Localizer {
    fn default() -> Self {
        Self::new(Language::English)
    }
}

impl Localizer {
    /// Load embedded resources once at an application boundary.
    pub fn new(language: Language) -> Self {
        let english = Arc::new(bundle(Language::English));
        if language == Language::English {
            Self {
                language,
                bundle: english,
                fallback: None,
                diagnostics: Arc::new(Vec::new()),
            }
        } else {
            Self {
                language,
                bundle: Arc::new(bundle(language)),
                diagnostics: Arc::new(diagnostic_templates(&english)),
                fallback: Some(english),
            }
        }
    }

    /// Current language.
    pub const fn language(&self) -> Language {
        self.language
    }

    /// Current language's canonical tag.
    pub const fn tag(&self) -> &'static str {
        self.language.tag()
    }

    /// Render a message without parameters.
    pub fn text(&self, id: &str) -> String {
        self.format(id, &[])
    }

    /// Render named text parameters. Values are never reparsed as Fluent.
    pub fn format(&self, id: &str, values: &[(&str, String)]) -> String {
        let mut args = FluentArgs::new();
        for (name, value) in values {
            args.set(*name, value.as_str());
        }
        self.resolve(id, Some(&args))
    }

    /// Render a single positional display value from an HTML template.
    pub fn one(&self, id: &str, value: impl fmt::Display) -> String {
        self.format(id, &[("value0", value.to_string())])
    }

    /// Render two positional display values from an HTML template.
    pub fn two(&self, id: &str, first: impl fmt::Display, second: impl fmt::Display) -> String {
        self.format(
            id,
            &[
                ("value0", first.to_string()),
                ("value1", second.to_string()),
            ],
        )
    }

    /// Render three positional display values from an HTML template.
    pub fn three(
        &self,
        id: &str,
        first: impl fmt::Display,
        second: impl fmt::Display,
        third: impl fmt::Display,
    ) -> String {
        self.format(
            id,
            &[
                ("value0", first.to_string()),
                ("value1", second.to_string()),
                ("value2", third.to_string()),
            ],
        )
    }

    /// HTML for a message whose `` `backtick` `` spans are code tokens. The
    /// text is escaped first, so the result is safe for askama's `safe` filter.
    pub fn rich(&self, id: &str, code_class: &str) -> String {
        Self::rich_html(&self.text(id), code_class)
    }

    /// [`Self::rich`] with one value, which cannot open or close a code span.
    pub fn rich_one(&self, id: &str, value: impl fmt::Display, code_class: &str) -> String {
        let value = value.to_string().replace('`', "\u{E000}");
        Self::rich_html(&self.one(id, value), code_class).replace('\u{E000}', "`")
    }

    fn rich_html(text: &str, code_class: &str) -> String {
        let escaped = text
            .replace('&', "&#38;")
            .replace('<', "&#60;")
            .replace('>', "&#62;")
            .replace('"', "&#34;")
            .replace('\'', "&#39;");
        let open = if code_class.is_empty() {
            "<code>".to_owned()
        } else {
            format!("<code class=\"{code_class}\">")
        };
        let mut html = String::with_capacity(escaped.len());
        for (index, part) in escaped.split('`').enumerate() {
            if index % 2 == 1 {
                html.push_str(&open);
                html.push_str(part);
                html.push_str("</code>");
            } else {
                html.push_str(part);
            }
        }
        html
    }

    /// Render typed Fluent parameters, including numbers used by plural rules.
    pub fn message<'a>(&self, id: &str, values: &[(&'a str, FluentValue<'a>)]) -> String {
        let mut args = FluentArgs::new();
        for (name, value) in values {
            args.set(*name, value.clone());
        }
        self.resolve(id, Some(&args))
    }

    /// Render a plural message using Fluent's locale-specific number rules.
    pub fn count<'a>(&self, id: &str, count: impl Into<FluentValue<'a>>) -> String {
        self.message(id, &[("count", count.into())])
    }

    /// Translate an application-owned status code at a display boundary.
    /// Unknown codes remain visible; user content must never pass through this method.
    pub fn status(&self, code: &str) -> String {
        let id = match code {
            "active" => "common-status-active",
            "interrupted" => "common-status-interrupted",
            "completed" => "common-status-completed",
            "empty" => "common-status-empty",
            "pending" => "common-status-pending",
            "claimed" => "common-status-claimed",
            "cancelled" => "common-status-cancelled",
            "editing" => "common-status-editing",
            "disabled" => "common-status-disabled",
            "unknown" => "common-status-unknown",
            "ok" => "common-status-ok",
            "error" => "common-status-error",
            "warn" => "common-status-warn",
            "open" => "common-status-open",
            "ended" => "common-status-ended",
            "revoked" => "common-status-revoked",
            _ => return code.to_owned(),
        };
        self.text(id)
    }

    /// Localize a fixed protocol error for display while preserving unknown diagnostics.
    pub fn native_error(&self, message: &str) -> String {
        FIXED_ERRORS
            .iter()
            .find(|(wire, _)| *wire == message)
            .map_or_else(|| message.to_owned(), |(_, id)| self.text(id))
    }

    /// Known protocol errors for progressive enhancement scripts. Unknown errors
    /// use the page's localized fallback; user content never becomes a message key.
    pub fn errors_js(&self) -> String {
        let errors = FIXED_ERRORS
            .iter()
            .map(|(wire, id)| (*wire, self.text(id)))
            .collect::<std::collections::BTreeMap<_, _>>();
        Self::escape_script(serde_json::to_string(&errors).unwrap_or_else(|_| "{}".into()))
    }

    /// Translate rendered English CLI text at the terminal boundary, keeping
    /// any text the catalog does not recognize, such as OS or provider details.
    pub fn diagnostic(&self, text: &str) -> String {
        self.diagnostic_at(text, 0)
            .unwrap_or_else(|| text.to_owned())
    }

    /// [`Self::diagnostic`] for askama, which cannot borrow an argument itself.
    pub fn label(&self, text: impl fmt::Display) -> String {
        self.diagnostic(&text.to_string())
    }

    /// Whether the catalog has a message matching this rendered English text.
    pub fn knows_diagnostic(&self, text: &str) -> bool {
        self.best_diagnostic(text).is_some()
    }

    fn best_diagnostic<'a>(
        &'a self,
        text: &'a str,
    ) -> Option<(&'a DiagnosticTemplate, Vec<(&'a str, &'a str)>)> {
        self.diagnostics
            .iter()
            .filter_map(|template| Some((template, template.capture(text)?)))
            .max_by_key(|(template, _)| template.literal_len)
    }

    fn diagnostic_at(&self, text: &str, depth: usize) -> Option<String> {
        if depth > MAX_DIAGNOSTIC_NESTING {
            return None;
        }
        if let Some((template, values)) = self.best_diagnostic(text) {
            let values = values
                .into_iter()
                .map(|(name, value)| {
                    let value = if NESTED_DIAGNOSTICS.contains(&name) {
                        self.diagnostic_at(value, depth + 1)
                            .unwrap_or_else(|| value.to_owned())
                    } else {
                        value.to_owned()
                    };
                    (name, value)
                })
                .collect::<Vec<_>>();
            return Some(self.format(&template.id, &values));
        }
        // `{error:#}` flattens a chain into "outer: inner".
        let (outer, inner) = text.split_once(": ")?;
        let outer = self.diagnostic_at(outer, depth + 1);
        let inner = self.diagnostic_at(inner, depth + 1);
        if outer.is_none() && inner.is_none() {
            return None;
        }
        let (original_outer, original_inner) = text.split_once(": ")?;
        Some(format!(
            "{}: {}",
            outer.unwrap_or_else(|| original_outer.to_owned()),
            inner.unwrap_or_else(|| original_inner.to_owned()),
        ))
    }

    /// Human-readable elapsed time, clamping future timestamps to "just now".
    pub fn age(&self, seconds: i64) -> String {
        let seconds = seconds.max(0);
        if seconds < 60 {
            return self.text("common-time-now");
        }
        let (count, unit) = match seconds {
            n if n < 3_600 => (n / 60, "common-time-minutes"),
            n if n < 86_400 => (n / 3_600, "common-time-hours"),
            n if n < 2_592_000 => (n / 86_400, "common-time-days"),
            n if n < 31_536_000 => (n / 2_592_000, "common-time-months"),
            n => (n / 31_536_000, "common-time-years"),
        };
        self.count(unit, count)
    }

    /// Relative ISO timestamp; malformed values remain visible as supplied.
    pub fn relative(&self, iso: &str) -> String {
        iso.parse::<jiff::Timestamp>().map_or_else(
            |_| iso.to_owned(),
            |then| self.age((jiff::Timestamp::now() - then).get_seconds()),
        )
    }

    /// JSON string literal safe in an inline HTML script, including `</script>`.
    pub fn js(&self, id: &str) -> String {
        Self::script_literal(&self.text(id))
    }

    /// Script-safe message with one opaque substitution marker.
    pub fn js_one(&self, id: &str, value: &str) -> String {
        Self::script_literal(&self.one(id, value))
    }

    /// Script-safe message with two opaque substitution markers.
    pub fn js_two(&self, id: &str, first: &str, second: &str) -> String {
        Self::script_literal(&self.two(id, first, second))
    }

    fn script_literal(text: &str) -> String {
        // Serializing a string cannot fail; retain a valid JS string even if
        // serde_json ever introduces a fallible string serializer.
        Self::escape_script(serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into()))
    }

    fn escape_script(json: String) -> String {
        json.replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('&', "\\u0026")
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029")
    }

    fn resolve(&self, id: &str, args: Option<&FluentArgs<'_>>) -> String {
        for bundle in std::iter::once(&self.bundle).chain(self.fallback.iter()) {
            if let Some(pattern) = bundle.get_message(id).and_then(|message| message.value()) {
                let mut errors = Vec::new();
                let text = bundle.format_pattern(pattern, args, &mut errors);
                if errors.is_empty() {
                    return text.into_owned();
                }
                tracing::warn!(message_id = id, "translation formatting failed");
            }
        }
        tracing::warn!(message_id = id, "translation unavailable");
        id.to_owned()
    }
}

fn bundle(language: Language) -> FluentBundle<FluentResource> {
    let tag = match language {
        Language::English => unic_langid::LanguageIdentifier::from_str("en").unwrap_or_default(),
        Language::PortugueseBrazil => {
            unic_langid::LanguageIdentifier::from_str("pt-BR").unwrap_or_default()
        }
    };
    let mut bundle = FluentBundle::new_concurrent(vec![tag]);
    // Paths, flags and identifiers must remain copyable in terminal output.
    // Web templates escape values at the final HTML boundary.
    bundle.set_use_isolating(false);
    for source in sources(language) {
        let resource = match FluentResource::try_new(source.to_owned()) {
            Ok(resource) => resource,
            Err((resource, _)) => {
                tracing::error!(
                    language = language.tag(),
                    "invalid embedded translation catalog"
                );
                resource
            }
        };
        if bundle.add_resource(resource).is_err() {
            tracing::error!(
                language = language.tag(),
                "duplicate embedded translation message"
            );
        }
    }
    bundle
}

fn diagnostic_templates(english: &FluentBundle<FluentResource>) -> Vec<DiagnosticTemplate> {
    const OPEN: char = '\u{E000}';
    const CLOSE: char = '\u{E001}';
    let Ok(resource) = parser::parse(include_str!("../locales/en/diagnostics.ftl")) else {
        tracing::error!("invalid embedded diagnostic catalog");
        return Vec::new();
    };
    let mut templates = Vec::new();
    for entry in resource.body {
        let ast::Entry::Message(message) = entry else {
            continue;
        };
        let Some(pattern) = message.value else {
            continue;
        };
        let mut args = FluentArgs::new();
        for element in &pattern.elements {
            if let ast::PatternElement::Placeable {
                expression: ast::Expression::Inline(ast::InlineExpression::VariableReference { id }),
            } = element
            {
                args.set(id.name, format!("{OPEN}{}{CLOSE}", id.name));
            }
        }
        let Some(value) = english
            .get_message(message.id.name)
            .and_then(|message| message.value())
        else {
            continue;
        };
        let mut errors = Vec::new();
        let rendered = english.format_pattern(value, Some(&args), &mut errors);
        let mut literals = Vec::new();
        let mut variables = Vec::new();
        let mut rest = rendered.as_ref();
        while let Some((literal, tail)) = rest.split_once(OPEN) {
            let Some((name, tail)) = tail.split_once(CLOSE) else {
                break;
            };
            literals.push(literal.to_owned());
            variables.push(name.to_owned());
            rest = tail;
        }
        literals.push(rest.to_owned());
        templates.push(DiagnosticTemplate {
            id: message.id.name.to_owned(),
            literal_len: literals.iter().map(String::len).sum(),
            literals,
            variables,
        });
    }
    templates
}

fn sources(language: Language) -> [&'static str; 5] {
    match language {
        Language::English => [
            include_str!("../locales/en/common.ftl"),
            include_str!("../locales/en/web.ftl"),
            include_str!("../locales/en/cli.ftl"),
            include_str!("../locales/en/cli-help.ftl"),
            include_str!("../locales/en/diagnostics.ftl"),
        ],
        Language::PortugueseBrazil => [
            include_str!("../locales/pt-BR/common.ftl"),
            include_str!("../locales/pt-BR/web.ftl"),
            include_str!("../locales/pt-BR/cli.ftl"),
            include_str!("../locales/pt-BR/cli-help.ftl"),
            include_str!("../locales/pt-BR/diagnostics.ftl"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn variables(pattern: &ast::Pattern<&str>) -> BTreeSet<String> {
        fn inline(value: &ast::InlineExpression<&str>, names: &mut BTreeSet<String>) {
            if let ast::InlineExpression::VariableReference { id } = value {
                names.insert(id.name.to_owned());
            }
        }
        let mut names = BTreeSet::new();
        for element in &pattern.elements {
            if let ast::PatternElement::Placeable { expression } = element {
                match expression {
                    ast::Expression::Inline(value) => inline(value, &mut names),
                    ast::Expression::Select { selector, variants } => {
                        inline(selector, &mut names);
                        for variant in variants {
                            names.extend(variables(&variant.value));
                        }
                    }
                }
            }
        }
        names
    }

    fn catalog(language: Language) -> BTreeMap<String, BTreeSet<String>> {
        let mut messages = BTreeMap::new();
        let localizer = Localizer::new(language);
        for source in sources(language) {
            let resource = parser::parse(source)
                .unwrap_or_else(|(_, errors)| panic!("{}: {errors:?}", language.tag()));
            for entry in resource.body {
                if let ast::Entry::Message(message) = entry {
                    let pattern = message.value.expect("messages have a value");
                    let vars = variables(&pattern);
                    let values = vars
                        .iter()
                        .map(|name| {
                            (
                                name.as_str(),
                                "example <script>{ $other } & \"ç\"".to_owned(),
                            )
                        })
                        .collect::<Vec<_>>();
                    let formatted = localizer.format(message.id.name, &values);
                    assert_ne!(
                        formatted,
                        message.id.name,
                        "{} {} failed to format",
                        language.tag(),
                        message.id.name
                    );
                    assert!(
                        messages.insert(message.id.name.to_owned(), vars).is_none(),
                        "duplicate {}",
                        message.id.name
                    );
                }
            }
        }
        messages
    }

    #[test]
    fn embedded_catalogs_parse_have_matching_keys_and_parameters() {
        let english = catalog(Language::English);
        for language in Language::ALL {
            assert_eq!(english, catalog(*language));
        }
    }

    #[test]
    fn language_aliases_and_posix_locale_spellings() {
        for value in ["pt-BR", "pt_BR.UTF-8", "pt", "PT-br", "pt_BR@variant"] {
            assert_eq!(value.parse(), Ok(Language::PortugueseBrazil));
        }
        for value in ["en", "en_US.UTF-8", "C", "C.UTF-8", "POSIX"] {
            assert_eq!(value.parse(), Ok(Language::English));
        }
        assert!("fr".parse::<Language>().is_err());
    }

    #[test]
    fn negotiation_honors_weights_ties_exclusions_and_invalid_values() {
        for (header, expected) in [
            ("pt-BR, en;q=0.8", Language::PortugueseBrazil),
            ("en;q=0.9,pt-BR;q=1", Language::PortugueseBrazil),
            ("pt;q=0.5,en;q=0.5", Language::PortugueseBrazil),
            ("pt-BR;q=0,en;q=0.1", Language::English),
            ("fr,pt-BR;q=0.8", Language::PortugueseBrazil),
            ("fr, *;q=0.1", Language::English),
            ("pt-BR;q=NaN", Language::English),
            ("pt-BR;q=2", Language::English),
            ("pt-BR;q=oops", Language::English),
            ("", Language::English),
        ] {
            assert_eq!(Language::negotiate(header), expected, "{header}");
        }
        assert_eq!(
            Language::negotiate(&"pt-BR,".repeat(1000)),
            Language::English
        );
    }

    #[test]
    fn independent_localizers_do_not_change_one_another() {
        let en = Localizer::default();
        let pt = Localizer::new(Language::PortugueseBrazil);
        assert_eq!(en.text("web-search"), "Search");
        assert_eq!(pt.text("web-search"), "Pesquisar");
        assert_eq!(en.text("web-search"), "Search");
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Localizer>();
    }

    #[test]
    fn parameters_are_opaque_and_script_literals_cannot_close_script_tags() {
        let localizer = Localizer::new(Language::PortugueseBrazil);
        let value = "</script><script>alert('x')</script>{ $other }";
        let text = localizer.one("web-diff-from-value0", value);
        assert!(text.contains(value));
        assert!(!localizer.js("web-search").contains('<'));
        let encoded = localizer.js_one("web-diff-from-value0", value);
        assert!(!encoded.contains("</script>"));
        let decoded: String = serde_json::from_str(&encoded).unwrap();
        assert!(decoded.contains(value));
        assert_eq!(localizer.text("missing-message"), "missing-message");
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    #[test]
    fn portuguese_plural_forms_and_future_times() {
        let pt = Localizer::new(Language::PortugueseBrazil);
        assert_eq!(pt.count("web-count-session", 1), "sessão");
        assert_eq!(pt.count("web-count-session", 2), "sessões");
        assert_eq!(pt.count("web-count-version", 2), "versões");
        assert_eq!(pt.age(60), "há 1 minuto");
        assert_eq!(pt.age(120), "há 2 minutos");
        assert_eq!(pt.age(2 * 2_592_000), "há 2 meses");
        assert_eq!(pt.age(-10), "agora mesmo");
        assert_eq!(
            pt.native_error("invalid credentials"),
            "Credenciais inválidas"
        );
        assert_eq!(
            pt.native_error("provider detail: keep exact"),
            "provider detail: keep exact"
        );
    }

    #[test]
    fn diagnostics_translate_rendered_text_and_keep_unknown_details() {
        let pt = Localizer::new(Language::PortugueseBrazil);
        assert_eq!(pt.diagnostic("reading /tmp/a b"), "lendo /tmp/a b");
        // The longest literal wins over the generic `reading { $path }`.
        assert_eq!(
            pt.diagnostic("reading the token from stdin"),
            "lendo o token da entrada padrão"
        );
        assert_eq!(
            pt.diagnostic("server returned 500: oops"),
            "o servidor retornou 500: oops"
        );
        // Values are opaque unless their name marks a nested diagnostic.
        assert_eq!(
            pt.diagnostic("ai-memory: warning: disk full"),
            "ai-memory: aviso: disk full"
        );
        assert_eq!(
            pt.diagnostic("ai-memory: could not read the post-run choice (interrupted); exiting"),
            "ai-memory: não foi possível ler a escolha pós-execução (interrompido); encerrando"
        );
        assert_eq!(pt.diagnostic("provider says no"), "provider says no");
        assert_eq!(
            pt.diagnostic("building HTTP client: dns failure"),
            "criando o cliente HTTP: dns failure"
        );
        let en = Localizer::default();
        assert_eq!(en.diagnostic("reading /tmp/a"), "reading /tmp/a");
    }

    #[test]
    fn rich_messages_escape_everything_before_adding_code_spans() {
        let pt = Localizer::new(Language::PortugueseBrazil);
        let html = pt.rich_one(
            "web-search-unknown-kind",
            "<script>`alert('x')`</script>",
            "font-mono",
        );
        assert!(html.starts_with("<code class=\"font-mono\">&#60;script&#62;`alert"));
        assert!(html.contains("&#60;/script&#62;</code>"));
        assert!(!html.contains("<script"));
        assert_eq!(
            pt.rich("web-mailbox-agent-reads", ""),
            "Um agente neste projeto as lê com <code>memory_message_pop</code>."
        );
    }

    #[test]
    fn diagnostic_values_are_never_reparsed() {
        let pt = Localizer::new(Language::PortugueseBrazil);
        assert_eq!(
            pt.diagnostic("reading { $other } \u{E000}x\u{E001}"),
            "lendo { $other } \u{E000}x\u{E001}"
        );
    }

    #[test]
    fn plural_suffixes_in_english_diagnostics_select_portuguese_phrases() {
        let pt = Localizer::new(Language::PortugueseBrazil);
        assert!(
            pt.diagnostic(
                "ai-memory hook-drain: recovered 1, dropped 0 malformed, left 1 unreadable temp entry"
            )
            .ends_with("1 entrada temporária ilegível mantida")
        );
        assert!(
            pt.diagnostic(
                "ai-memory hook-drain: recovered 1, dropped 0 malformed, left 2 unreadable temp entries"
            )
            .ends_with("2 entradas temporárias ilegíveis mantidas")
        );
    }

    #[test]
    fn an_incomplete_locale_falls_back_to_english() {
        let en = Localizer::default();
        let empty = FluentBundle::new_concurrent(vec!["pt-BR".parse().unwrap()]);
        let pt = Localizer {
            language: Language::PortugueseBrazil,
            bundle: Arc::new(empty),
            fallback: Some(en.bundle),
            diagnostics: Arc::new(Vec::new()),
        };
        assert_eq!(pt.text("web-search"), "Search");
    }

    #[test]
    fn literal_message_ids_exist_in_the_reference_catalog() {
        fn scan(
            path: &std::path::Path,
            pattern: &regex::Regex,
            bundle: &FluentBundle<FluentResource>,
        ) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    scan(&path, pattern, bundle);
                } else if path
                    .extension()
                    .is_some_and(|ext| ext == "rs" || ext == "html")
                {
                    let source = std::fs::read_to_string(&path).unwrap();
                    for captures in pattern.captures_iter(&source) {
                        let id = &captures[1];
                        assert!(
                            bundle.has_message(id),
                            "{}: missing message {id}",
                            path.display()
                        );
                    }
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let pattern = regex::Regex::new(r#"\.(?:text|format|message|one|two|three|rich|rich_one|count|js|js_one|js_two)\(\s*"((?:common|cli|web|setup-web)-[a-z0-9-]+)""#).unwrap();
        let en = Localizer::default();
        for path in [
            "crates/ai-memory-core/src",
            "crates/ai-memory-cli/src",
            "crates/ai-memory-cli/templates",
            "crates/ai-memory-web/src",
            "crates/ai-memory-web/templates",
        ] {
            scan(&root.join(path), &pattern, &en.bundle);
        }
    }
}
