//! Product documentation embedded with the binary, independent of project memory.

use crate::markdown::HeadingSlugger;
use ai_memory_core::i18n::Language;
use serde::Serialize;

/// Directory holding the Brazilian Portuguese translations of the guides in `docs/`.
const PT_BR_DIR: &str = "docs/pt-BR/";

#[derive(Clone, Copy, Serialize)]
pub(crate) struct Document {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub source: &'static str,
    #[serde(skip)]
    pub body: &'static str,
    /// Repository path of the pt-BR translation, when the guide has one.
    #[serde(skip)]
    pub source_pt_br: Option<&'static str>,
    #[serde(skip)]
    pub body_pt_br: Option<&'static str>,
}

impl Document {
    /// The guide in the reader's language; guides without a translation stay in English.
    pub fn body_for(&self, language: Language) -> &'static str {
        match language {
            Language::PortugueseBrazil => self.body_pt_br.unwrap_or(self.body),
            Language::English => self.body,
        }
    }

    /// Repository path of the text [`Self::body_for`] returns, so relative links resolve from it.
    pub fn source_for(&self, language: Language) -> &'static str {
        match language {
            Language::PortugueseBrazil => self.source_pt_br.unwrap_or(self.source),
            Language::English => self.source,
        }
    }
}

macro_rules! document {
    ($id:literal, $title:literal, $description:literal, $category:literal, $source:literal) => {
        Document {
            id: $id,
            title: $title,
            description: $description,
            category: $category,
            source: $source,
            body: include_str!(concat!("../../../", $source)),
            source_pt_br: None,
            body_pt_br: None,
        }
    };
    ($id:literal, $title:literal, $description:literal, $category:literal, $source:literal, pt $pt:literal) => {
        Document {
            id: $id,
            title: $title,
            description: $description,
            category: $category,
            source: $source,
            body: include_str!(concat!("../../../", $source)),
            source_pt_br: Some($pt),
            body_pt_br: Some(include_str!(concat!("../../../", $pt))),
        }
    };
}

pub(crate) const DOCUMENTS: &[Document] = &[
    document!(
        "features",
        "Features & activation",
        "What each feature does, prerequisites, how to enable it, and how to verify capture and access.",
        "Start here",
        "docs/features.md",
        pt "docs/pt-BR/features.md"
    ),
    document!(
        "install",
        "Installation & agent setup",
        "Native and Docker setup, client hooks, MCP, supported harnesses, and upgrades.",
        "Start here",
        "docs/install.md",
        pt "docs/pt-BR/install.md"
    ),
    document!(
        "usage",
        "Usage & dashboard",
        "Recall, handoffs, recovery, live token accounting, agent sessions, and the web inbox.",
        "Start here",
        "docs/usage.md",
        pt "docs/pt-BR/usage.md"
    ),
    document!(
        "cookbook",
        "Task cookbook",
        "Practical recipes for capturing, recalling, importing, and coordinating work.",
        "Start here",
        "docs/cookbook.md",
        pt "docs/pt-BR/cookbook.md"
    ),
    document!(
        "agent-messaging",
        "Agent inbox & messaging",
        "Shared and directed messages, browser sending, recipient identity, and claim-once delivery.",
        "Agent workflows",
        "docs/agent-messaging.md",
        pt "docs/pt-BR/agent-messaging.md"
    ),
    document!(
        "managed-workstreams",
        "Managed sessions & workstreams",
        "Launch and resume agents with ai-memory run, profiles, auto-wiring, and cross-harness continuity.",
        "Agent workflows",
        "docs/managed-workstreams.md",
        pt "docs/pt-BR/managed-workstreams.md"
    ),
    document!(
        "marker-file",
        "Project routing & capture privacy",
        "Configure workspace/project identity and capture exclusions in .ai-memory.toml.",
        "Agent workflows",
        "docs/marker-file.md",
        pt "docs/pt-BR/marker-file.md"
    ),
    document!(
        "programmatic-memory",
        "Programmatic memory",
        "Use the public MCP protocol from applications and orchestrators.",
        "Agent workflows",
        "docs/programmatic-memory.md",
        pt "docs/pt-BR/programmatic-memory.md"
    ),
    document!(
        "external-lifecycle",
        "External lifecycle capture",
        "Connect orchestrators and the relay through the bounded lifecycle ingestion API.",
        "Agent workflows",
        "docs/external-lifecycle.md",
        pt "docs/pt-BR/external-lifecycle.md"
    ),
    document!(
        "llm-providers",
        "LLM & embedding providers",
        "Enable optional provider-backed consolidation, embeddings, authentication, and fallback chains.",
        "Learning & retrieval",
        "docs/llm-providers.md",
        pt "docs/pt-BR/llm-providers.md"
    ),
    document!(
        "llm-provider-fallback",
        "Provider fallback",
        "Configure an ordered provider chain and understand which failures may trigger fallback.",
        "Learning & retrieval",
        "docs/llm-provider-fallback.md",
        pt "docs/pt-BR/llm-provider-fallback.md"
    ),
    document!(
        "okf",
        "Portable wiki format",
        "Understand Markdown frontmatter, portable wiki layout, import, and format compatibility.",
        "Agent workflows",
        "docs/okf.md",
        pt "docs/pt-BR/okf.md"
    ),
    document!(
        "session-management",
        "Session management API",
        "Build and enable owned-session inspection and purge routes with human authentication.",
        "Access & integration",
        "docs/session-management.md",
        pt "docs/pt-BR/session-management.md"
    ),
    document!(
        "self-service-keys",
        "Self-service API keys",
        "Build and enable the optional API for users to manage their own native credentials.",
        "Access & integration",
        "docs/self-service-keys.md",
        pt "docs/pt-BR/self-service-keys.md"
    ),
    document!(
        "auto-improvement-loop",
        "Learning & proposal review",
        "Current learning behavior, approval settings, scheduler prerequisites, and detailed design history.",
        "Learning & retrieval",
        "docs/auto-improvement-loop.md",
        pt "docs/pt-BR/auto-improvement-loop.md"
    ),
    document!(
        "architecture",
        "Configuration & architecture",
        "The complete configuration reference, storage, retrieval signals, APIs, and operating boundaries.",
        "Operations",
        "docs/ARCHITECTURE.md",
        pt "docs/pt-BR/ARCHITECTURE.md"
    ),
    document!(
        "diagnostics",
        "Diagnostics & health",
        "Inspect capture, providers, logging, and server health when a feature is not reporting data.",
        "Operations",
        "docs/diagnostics.md",
        pt "docs/pt-BR/diagnostics.md"
    ),
    document!(
        "lifecycle-ops",
        "Backup, recovery & lifecycle",
        "Online backups, restore requirements, retention, moves, and scoped lifecycle operations.",
        "Operations",
        "docs/lifecycle-ops.md",
        pt "docs/pt-BR/lifecycle-ops.md"
    ),
    document!(
        "deploy",
        "Deployment",
        "Native and container deployment, environment settings, and persistent storage.",
        "Operations",
        "docs/deploy.md",
        pt "docs/pt-BR/deploy.md"
    ),
    document!(
        "https-via-proxy",
        "HTTPS & reverse proxy",
        "Configure TLS and proxy deployment while retaining authentication and host checks.",
        "Operations",
        "docs/https-via-proxy.md",
        pt "docs/pt-BR/https-via-proxy.md"
    ),
    document!(
        "management-ui",
        "Management console",
        "Build and enable the optional console, human login, session management, and API keys.",
        "Access & integration",
        "docs/management-ui.md",
        pt "docs/pt-BR/management-ui.md"
    ),
    document!(
        "security",
        "Security & authentication",
        "Authentication, authorization, sanitization, and deployment security guidance.",
        "Access & integration",
        "docs/security.md",
        pt "docs/pt-BR/security.md"
    ),
    document!(
        "sso",
        "SSO & enterprise identity",
        "OIDC device login, identity-provider configuration, and supported enterprise workflows.",
        "Access & integration",
        "docs/sso.md",
        pt "docs/pt-BR/sso.md"
    ),
    document!(
        "frontend-api",
        "HTTP API & agent-readable docs",
        "Read-only JSON endpoints, documentation access, custom frontends, and URL prefixes.",
        "Access & integration",
        "docs/frontend-api.md",
        pt "docs/pt-BR/frontend-api.md"
    ),
    document!(
        "changelog",
        "Changelog",
        "Unreleased changes and the current major release history, bundled with this build.",
        "Release history",
        "CHANGELOG.md"
    ),
    document!(
        "changelog-2x",
        "Changelog · 2.x",
        "Archived releases from the 2.x series.",
        "Release history",
        "docs/changelog/2.x.md"
    ),
    document!(
        "changelog-1x",
        "Changelog · 1.x",
        "Archived releases from the 1.x series.",
        "Release history",
        "docs/changelog/1.x.md"
    ),
    document!(
        "changelog-0x",
        "Changelog · 0.x",
        "Archived releases from the 0.x series.",
        "Release history",
        "docs/changelog/0.x.md"
    ),
];

pub(crate) fn find(id: &str) -> Option<&'static Document> {
    DOCUMENTS.iter().find(|doc| doc.id == id)
}

pub(crate) fn search(query: &str) -> Vec<&'static Document> {
    search_in(query, Language::English)
}

/// Search the guides in the reader's language; titles and descriptions stay English.
pub(crate) fn search_in(query: &str, language: Language) -> Vec<&'static Document> {
    let terms: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
    if terms.is_empty() {
        return DOCUMENTS.iter().collect();
    }
    DOCUMENTS
        .iter()
        .filter(|doc| {
            let text = format!(
                "{} {} {} {}",
                doc.title,
                doc.description,
                doc.category,
                doc.body_for(language)
            )
            .to_lowercase();
            terms.iter().all(|term| text.contains(term))
        })
        .collect()
}

pub(crate) struct Release<'a> {
    pub id: &'a str,
    pub heading: &'a str,
    pub body: &'a str,
}

pub(crate) fn releases(body: &str) -> Vec<Release<'_>> {
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("## [")
            && let Some((id, _)) = rest.split_once(']')
        {
            starts.push((offset, id, line.trim()));
        }
        offset += line.len();
    }
    starts
        .iter()
        .enumerate()
        .map(|(i, &(start, id, heading))| Release {
            id,
            heading,
            body: &body[start..starts.get(i + 1).map_or(body.len(), |s| s.0)],
        })
        .collect()
}

/// Heading ids of a guide, in document order, as the renderer will emit them.
fn heading_ids(body: &str) -> Vec<String> {
    headings(body).into_iter().map(|(_, id)| id).collect()
}

/// `(level, id)` of every heading outside code fences, in document order.
fn headings(body: &str) -> Vec<(usize, String)> {
    let mut slugger = HeadingSlugger::default();
    let mut fence: Option<(char, usize)> = None;
    let mut ids = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let run = |c: char| trimmed.chars().take_while(|&x| x == c).count();
        let marker = ['`', '~'].into_iter().find(|&c| run(c) >= 3 && indent <= 3);
        if let Some(c) = marker {
            let rest = trimmed.trim_start_matches(c);
            match fence {
                None if c != '`' || !rest.contains('`') => fence = Some((c, run(c))),
                Some((open, len)) if open == c && run(c) >= len && rest.trim().is_empty() => {
                    fence = None;
                }
                _ => {}
            }
            continue;
        }
        if fence.is_some() || indent > 3 {
            continue;
        }
        let level = run('#');
        let Some(text) = trimmed[level..]
            .strip_prefix(' ')
            .filter(|_| (1..=6).contains(&level))
        else {
            continue;
        };
        let id = slugger.slugify(&strip_link_targets(text.trim().trim_end_matches('#')));
        ids.push((level, id));
    }
    ids
}

/// Keep link text and drop `(target)`, which the renderer does not put in a heading id.
fn strip_link_targets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("](") {
        out.push_str(&rest[..at + 1]);
        match rest[at + 2..].find(')') {
            Some(close) => rest = &rest[at + 2 + close + 1..],
            None => return out + &rest[at + 1..],
        }
    }
    out + rest
}

/// Point a `guide#fragment` reference, written against the English headings, at the same
/// section of the translation the reader will get.
///
/// Translated headings have translated ids, so a fragment taken from the English guide would
/// land nowhere. Headings correspond one to one, so the section is found by position; when the
/// two guides disagree on structure the reference is left unchanged.
pub(crate) fn localize_guide(guide: &str, language: Language) -> String {
    let Some((id, fragment)) = guide.split_once('#') else {
        return guide.into();
    };
    let Some(doc) = find(id).filter(|_| language == Language::PortugueseBrazil) else {
        return guide.into();
    };
    let Some(translated) = doc.body_pt_br else {
        return guide.into();
    };
    let english = heading_ids(doc.body);
    let portuguese = heading_ids(translated);
    match english.iter().position(|h| h == fragment) {
        Some(at) if english.len() == portuguese.len() => format!("{id}#{}", portuguese[at]),
        _ => guide.into(),
    }
}

/// Resolve repository links from a guide read from `source` without interpreting them as
/// project wiki paths. `source` may be a translation under `docs/pt-BR/`.
///
/// A translation links to its siblings by the same relative names the English guide uses, so a
/// target inside `docs/pt-BR/` is either a translated guide or, when no translation exists, the
/// English file one directory up.
pub(crate) fn link_from(source: &str, current_href: &str, target: &str) -> String {
    if target.starts_with('#') {
        return format!("{current_href}{target}");
    }
    if target.starts_with('/') || target.contains(':') || target.starts_with('\\') {
        return target.into();
    }
    let split = target.find(['#', '?']).unwrap_or(target.len());
    let (path, suffix) = target.split_at(split);
    let mut parts: Vec<_> = source.split('/').collect();
    parts.pop();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return "#".into();
                }
            }
            part => parts.push(part),
        }
    }
    let mut normalized = parts.join("/");
    if normalized.starts_with(PT_BR_DIR) {
        if let Some(known) = DOCUMENTS
            .iter()
            .find(|d| d.source_pt_br == Some(normalized.as_str()))
        {
            return format!("docs/{}{suffix}", known.id);
        }
        // No translation: fall back to the English file next to the translations.
        parts.remove(1);
        normalized = parts.join("/");
    }
    if let Some(known) = DOCUMENTS
        .iter()
        .find(|d| d.source == normalized || d.source == path.trim_start_matches("./"))
    {
        return format!("docs/{}{suffix}", known.id);
    }
    if normalized == "docs/changelog" {
        return format!("docs/changelog{suffix}");
    }
    let encoded = parts
        .iter()
        .map(|p| crate::templates::encode_segment(p))
        .collect::<Vec<_>>()
        .join("/");
    format!(
        "https://github.com/akitaonrails/ai-memory/{}/main/{encoded}{suffix}",
        if path.ends_with('/') { "tree" } else { "blob" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_translation_keeps_the_heading_structure_of_the_english_guide() {
        for doc in DOCUMENTS {
            let Some(translated) = doc.body_pt_br else {
                continue;
            };
            let levels = |body| {
                headings(body)
                    .into_iter()
                    .map(|(l, _)| l)
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                levels(doc.body),
                levels(translated),
                "{}: anchors are remapped by heading position, so the heading levels must match",
                doc.id
            );
        }
    }

    #[test]
    fn guide_sections_follow_the_reader_language() {
        let pt = localize_guide(
            "features#token-accounting-and-investigation",
            Language::PortugueseBrazil,
        );
        assert_ne!(pt, "features#token-accounting-and-investigation");
        assert!(pt.starts_with("features#"));
        assert!(
            heading_ids(find("features").unwrap().body_pt_br.unwrap())
                .contains(&pt["features#".len()..].to_owned()),
            "{pt} must be a heading id of the translated guide"
        );
        assert_eq!(
            localize_guide(
                "features#token-accounting-and-investigation",
                Language::English
            ),
            "features#token-accounting-and-investigation"
        );
        // Unknown guide, unknown fragment and no fragment are passed through untouched.
        for same in ["nope#x", "features#not-a-heading", "features"] {
            assert_eq!(localize_guide(same, Language::PortugueseBrazil), same);
        }
        // A guide with no translation keeps its English fragment.
        assert_eq!(
            localize_guide("changelog#unreleased", Language::PortugueseBrazil),
            "changelog#unreleased"
        );
    }

    #[test]
    fn translated_guides_resolve_siblings_repository_files_and_untranslated_guides() {
        let from = "docs/pt-BR/usage.md";
        assert_eq!(
            link_from(from, "docs/usage", "install.md#x"),
            "docs/install#x"
        );
        assert_eq!(
            link_from(from, "docs/usage", "../../SECURITY.md"),
            "https://github.com/akitaonrails/ai-memory/blob/main/SECURITY.md"
        );
        assert_eq!(
            link_from(from, "docs/usage", "macos.md#scenario-d-menu-bar-app"),
            "https://github.com/akitaonrails/ai-memory/blob/main/docs/macos.md#scenario-d-menu-bar-app"
        );
        assert_eq!(
            link_from(from, "docs/usage", "#section"),
            "docs/usage#section"
        );
        // Escaping out of the repository is still refused.
        assert_eq!(link_from(from, "docs/usage", "../../../x.md"), "#");
    }
}
