use std::collections::{HashMap, HashSet};
use std::ops::Range;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const MAX_EDITS: usize = 32;
pub(crate) const MAX_EDIT_CHARS: usize = 64 * 1024;
pub(crate) const MAX_CHANGED_CHARS: usize = 256 * 1024;
pub(crate) const MAX_SECTION_HASHES: usize = 100;

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct SectionHash {
    pub anchor: String,
    pub level: u8,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AppendTarget {
    Section,
    EndOfPage,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum PagePatchEdit {
    Append {
        target: AppendTarget,
        #[serde(default)]
        anchor: Option<String>,
        content: String,
    },
    AddSection {
        anchor: String,
        content: String,
    },
    ReplaceSection {
        anchor: String,
        section_sha256: String,
        content: String,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum PatchError {
    #[error("at least one edit is required")]
    Empty,
    #[error("too many edits (maximum {MAX_EDITS})")]
    TooMany,
    #[error("edit content is too large")]
    ContentTooLarge,
    #[error("combined edit content is too large")]
    BatchTooLarge,
    #[error("append target section requires an anchor")]
    MissingAnchor,
    #[error("append target end_of_page cannot have an anchor")]
    UnexpectedAnchor,
    #[error("anchor not found: {0}")]
    AnchorNotFound(String),
    #[error("anchor is ambiguous: {0}")]
    AmbiguousAnchor(String),
    #[error("more than one edit targets anchor: {0}")]
    DuplicateTarget(String),
    #[error("add_section anchor must be one ATX level 2-6 heading")]
    InvalidNewHeading,
    #[error("heading already exists: {0}")]
    HeadingExists(String),
    #[error("section precondition failed: {0}")]
    SectionPreconditionFailed(String),
    #[error("edits overlap")]
    Overlapping,
}

#[derive(Debug, Clone)]
struct Section {
    anchor: String,
    normalized: String,
    level: u8,
    whole: Range<usize>,
    content: Range<usize>,
}

/// Where an insertion goes among others at the same offset. Every edit
/// resolves against the captured body, so text for an existing section comes
/// first, then an `end_of_page` append, then new headings. Request order alone
/// would put a section append that follows an `add_section` under the new
/// heading whenever both land at the end of the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Placement {
    Section,
    EndOfPage,
    NewSection,
}

#[derive(Debug)]
struct Change {
    range: Range<usize>,
    replacement: String,
    placement: Placement,
    /// Where the heading of the existing section this edit writes into
    /// starts; `None` for an `end_of_page` append and a new section.
    heading: Option<usize>,
}

pub(crate) fn section_hashes(body: &str) -> (Vec<SectionHash>, bool) {
    let sections = parse_sections(body);
    let truncated = sections.len() > MAX_SECTION_HASHES;
    let hashes = sections
        .into_iter()
        .take(MAX_SECTION_HASHES)
        .map(|section| SectionHash {
            anchor: section.anchor,
            level: section.level,
            sha256: hash(&body.as_bytes()[section.whole]),
        })
        .collect();
    (hashes, truncated)
}

pub(crate) fn apply(body: &str, edits: &[PagePatchEdit]) -> Result<String, PatchError> {
    validate_bounds(edits)?;
    let sections = parse_sections(body);
    let mut by_anchor: HashMap<&str, Vec<&Section>> = HashMap::new();
    for section in &sections {
        by_anchor
            .entry(section.normalized.as_str())
            .or_default()
            .push(section);
    }

    let mut targets = HashSet::new();
    let mut added = HashSet::new();
    let mut changes = Vec::with_capacity(edits.len());
    for edit in edits {
        let change = match edit {
            PagePatchEdit::Append {
                target: AppendTarget::EndOfPage,
                anchor,
                content,
            } => {
                if anchor
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
                {
                    return Err(PatchError::UnexpectedAnchor);
                }
                if !targets.insert("\0eof".to_string()) {
                    return Err(PatchError::DuplicateTarget("end_of_page".into()));
                }
                Change {
                    range: body.len()..body.len(),
                    replacement: appended(body, content),
                    placement: Placement::EndOfPage,
                    heading: None,
                }
            }
            PagePatchEdit::Append {
                target: AppendTarget::Section,
                anchor,
                content,
            } => {
                let raw = anchor.as_deref().ok_or(PatchError::MissingAnchor)?;
                let section = find_section(&by_anchor, raw)?;
                if !targets.insert(section.normalized.clone()) {
                    return Err(PatchError::DuplicateTarget(section.anchor.clone()));
                }
                Change {
                    range: section.whole.end..section.whole.end,
                    replacement: appended(&body[section.whole.clone()], content),
                    placement: Placement::Section,
                    heading: Some(section.whole.start),
                }
            }
            PagePatchEdit::AddSection { anchor, content } => {
                let (level, title) = parse_single_heading(anchor)?;
                let normalized = normalize_anchor(&title);
                if by_anchor.contains_key(normalized.as_str()) || !added.insert(normalized.clone())
                {
                    return Err(PatchError::HeadingExists(title));
                }
                targets.insert(format!("\0add:{normalized}"));
                let heading = format!("{} {}", "#".repeat(level as usize), title);
                Change {
                    range: body.len()..body.len(),
                    replacement: appended(body, &format!("{heading}\n{content}")),
                    placement: Placement::NewSection,
                    heading: None,
                }
            }
            PagePatchEdit::ReplaceSection {
                anchor,
                section_sha256,
                content,
            } => {
                let section = find_section(&by_anchor, anchor)?;
                if !targets.insert(section.normalized.clone()) {
                    return Err(PatchError::DuplicateTarget(section.anchor.clone()));
                }
                if !constant_time_eq(
                    section_sha256.trim(),
                    &hash(&body.as_bytes()[section.whole.clone()]),
                ) {
                    return Err(PatchError::SectionPreconditionFailed(
                        section.anchor.clone(),
                    ));
                }
                Change {
                    range: section.content.clone(),
                    replacement: section_content(content),
                    placement: Placement::Section,
                    heading: Some(section.whole.start),
                }
            }
        };
        changes.push(change);
    }

    // A replacement deletes every section nested in it, so no other edit may
    // write into one of those. An append to the last nested section inserts
    // exactly where the replaced range ends, which the range check below lets
    // through.
    if changes.iter().any(|replaced| {
        changes
            .iter()
            .filter_map(|change| change.heading)
            .any(|heading| replaced.range.contains(&heading))
    }) {
        return Err(PatchError::Overlapping);
    }

    changes.sort_by_key(|change| (change.range.start, change.range.end, change.placement));
    let mut merged: Vec<Change> = Vec::with_capacity(changes.len());
    for change in changes {
        if let Some(previous) = merged.last_mut()
            && previous.range.is_empty()
            && change.range == previous.range
        {
            previous.replacement.push_str(&change.replacement);
            continue;
        }
        if merged
            .last()
            .is_some_and(|previous| previous.range.end > change.range.start)
        {
            return Err(PatchError::Overlapping);
        }
        merged.push(change);
    }
    let mut result = body.to_owned();
    for change in merged.into_iter().rev() {
        result.replace_range(change.range, &change.replacement);
    }
    Ok(result)
}

fn validate_bounds(edits: &[PagePatchEdit]) -> Result<(), PatchError> {
    if edits.is_empty() {
        return Err(PatchError::Empty);
    }
    if edits.len() > MAX_EDITS {
        return Err(PatchError::TooMany);
    }
    let sizes = edits.iter().map(|edit| match edit {
        PagePatchEdit::Append { content, .. }
        | PagePatchEdit::AddSection { content, .. }
        | PagePatchEdit::ReplaceSection { content, .. } => content.chars().count(),
    });
    let mut total = 0usize;
    for size in sizes {
        if size > MAX_EDIT_CHARS {
            return Err(PatchError::ContentTooLarge);
        }
        total = total.saturating_add(size);
    }
    if total > MAX_CHANGED_CHARS {
        return Err(PatchError::BatchTooLarge);
    }
    Ok(())
}

fn find_section<'a>(
    sections: &'a HashMap<&str, Vec<&Section>>,
    anchor: &str,
) -> Result<&'a Section, PatchError> {
    let normalized = normalize_anchor(anchor);
    match sections.get(normalized.as_str()).map(Vec::as_slice) {
        None | Some([]) => Err(PatchError::AnchorNotFound(anchor.to_owned())),
        Some([section]) => Ok(section),
        Some(_) => Err(PatchError::AmbiguousAnchor(anchor.to_owned())),
    }
}

fn parse_sections(body: &str) -> Vec<Section> {
    let mut headings = Vec::new();
    let mut offset = 0usize;
    let mut fence: Option<(char, usize)> = None;
    for line_with_newline in body.split_inclusive('\n') {
        let line = line_with_newline
            .strip_suffix('\n')
            .unwrap_or(line_with_newline);
        if let Some((marker, width)) = fence {
            if closes_fence(line, marker, width) {
                fence = None;
            }
        } else if let Some(marker) = fence_marker(line) {
            fence = Some(marker);
        } else if let Some((level, anchor)) = heading(line) {
            headings.push((offset, offset + line_with_newline.len(), level, anchor));
        }
        offset += line_with_newline.len();
    }
    headings
        .iter()
        .enumerate()
        .map(|(index, (start, content_start, level, anchor))| {
            let end = headings[index + 1..]
                .iter()
                .find(|(_, _, next_level, _)| next_level <= level)
                .map(|(next_start, _, _, _)| *next_start)
                .unwrap_or(body.len());
            Section {
                anchor: anchor.clone(),
                normalized: normalize_anchor(anchor),
                level: *level,
                whole: *start..end,
                content: *content_start..end,
            }
        })
        .collect()
}

fn heading(line: &str) -> Option<(u8, String)> {
    let trimmed = line
        .strip_prefix("   ")
        .unwrap_or(line.trim_start_matches(' '));
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let hashes = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&hashes)
        || !trimmed
            .as_bytes()
            .get(hashes)
            .is_none_or(u8::is_ascii_whitespace)
    {
        return None;
    }
    let mut title = trimmed[hashes..].trim().to_owned();
    // A trailing `#` run closes the heading only after a space or tab
    // (CommonMark), so `## C#` keeps its `#`.
    let without_closing = title.trim_end_matches('#');
    if without_closing.is_empty() || without_closing.ends_with([' ', '\t']) {
        title = without_closing.trim_end().to_owned();
    }
    (!title.is_empty()).then_some((hashes as u8, title))
}

fn fence_marker(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let width = trimmed.chars().take_while(|c| *c == marker).count();
    (width >= 3).then_some((marker, width))
}

fn closes_fence(line: &str, marker: char, width: usize) -> bool {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return false;
    }
    let count = trimmed
        .chars()
        .take_while(|candidate| *candidate == marker)
        .count();
    count >= width && trimmed[count..].trim().is_empty()
}

fn parse_single_heading(raw: &str) -> Result<(u8, String), PatchError> {
    if raw.contains('\n') {
        return Err(PatchError::InvalidNewHeading);
    }
    let (level, title) = heading(raw).ok_or(PatchError::InvalidNewHeading)?;
    if level == 1 {
        return Err(PatchError::InvalidNewHeading);
    }
    Ok((level, title))
}

fn normalize_anchor(raw: &str) -> String {
    let title = heading(raw)
        .map(|(_, title)| title)
        .unwrap_or_else(|| raw.to_owned());
    title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn appended(existing: &str, content: &str) -> String {
    let prefix = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let suffix = if content.ends_with('\n') { "" } else { "\n" };
    format!("{prefix}{content}{suffix}")
}

fn section_content(content: &str) -> String {
    let suffix = if content.is_empty() || content.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!("{content}{suffix}")
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |different, (a, b)| different | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_ignore_headings_inside_fences_and_flag_duplicates() {
        let body = "# T\n## Same\na\n```md\n## Hidden\n```\n## Same\nb\n";
        let (sections, truncated) = section_hashes(body);
        assert!(!truncated);
        assert_eq!(sections.iter().filter(|s| s.anchor == "Same").count(), 2);
        assert!(!sections.iter().any(|s| s.anchor == "Hidden"));
        assert_eq!(
            apply(
                body,
                &[PagePatchEdit::Append {
                    target: AppendTarget::Section,
                    anchor: Some("same".into()),
                    content: "x".into(),
                }]
            ),
            Err(PatchError::AmbiguousAnchor("same".into()))
        );
    }

    #[test]
    fn replaces_one_section_only_with_its_hash() {
        let body = "# T\n## One\nold\n### Child\nkept in section\n## Two\nother\n";
        let hash = section_hashes(body)
            .0
            .into_iter()
            .find(|section| section.anchor == "One")
            .unwrap()
            .sha256;
        let patched = apply(
            body,
            &[PagePatchEdit::ReplaceSection {
                anchor: "## One".into(),
                section_sha256: hash,
                content: "new".into(),
            }],
        )
        .unwrap();
        assert_eq!(patched, "# T\n## One\nnew\n## Two\nother\n");
    }

    #[test]
    fn stale_hash_and_conflicting_targets_fail_without_output() {
        let body = "# T\n## One\nold\n";
        assert!(matches!(
            apply(
                body,
                &[PagePatchEdit::ReplaceSection {
                    anchor: "One".into(),
                    section_sha256: "0".repeat(64),
                    content: "new".into(),
                }]
            ),
            Err(PatchError::SectionPreconditionFailed(_))
        ));
        let edits = [
            PagePatchEdit::Append {
                target: AppendTarget::Section,
                anchor: Some("One".into()),
                content: "a".into(),
            },
            PagePatchEdit::Append {
                target: AppendTarget::Section,
                anchor: Some("one".into()),
                content: "b".into(),
            },
        ];
        assert!(matches!(
            apply(body, &edits),
            Err(PatchError::DuplicateTarget(_))
        ));
    }

    #[test]
    fn appends_to_section_or_page_and_adds_a_unique_section() {
        let body = "# T\n## Log\none\n## Tail\ntwo";
        let patched = apply(
            body,
            &[
                PagePatchEdit::Append {
                    target: AppendTarget::Section,
                    anchor: Some("Log".into()),
                    content: "next".into(),
                },
                PagePatchEdit::AddSection {
                    anchor: "## Fresh".into(),
                    content: "new".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            patched,
            "# T\n## Log\none\nnext\n## Tail\ntwo\n## Fresh\nnew\n"
        );
    }

    /// An append to the last section, an `end_of_page` append and a new
    /// section all insert at the end of the page. Each still lands where its
    /// anchor resolved in the captured body, whatever the request order.
    #[test]
    fn same_offset_edits_land_where_they_resolved_in_any_order() {
        let body = "# Status\n## Log\nold\n";
        let edits = [
            PagePatchEdit::Append {
                target: AppendTarget::Section,
                anchor: Some("Log".into()),
                content: "entry".into(),
            },
            PagePatchEdit::Append {
                target: AppendTarget::EndOfPage,
                anchor: None,
                content: "footer".into(),
            },
            PagePatchEdit::AddSection {
                anchor: "## Decisions".into(),
                content: "d1".into(),
            },
        ];
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let batch = order.map(|index| edits[index].clone());
            assert_eq!(
                apply(body, &batch).unwrap(),
                "# Status\n## Log\nold\nentry\nfooter\n## Decisions\nd1\n",
                "request order {order:?}"
            );
        }

        // New sections keep the order the request gave them.
        let added = |title: &str| PagePatchEdit::AddSection {
            anchor: format!("## {title}"),
            content: title.to_lowercase(),
        };
        assert_eq!(
            apply(body, &[added("Beta"), added("Alpha")]).unwrap(),
            "# Status\n## Log\nold\n## Beta\nbeta\n## Alpha\nalpha\n"
        );
    }

    /// Replacing a section deletes the sections nested in it, so an edit to
    /// one of them overlaps the replacement, also when it inserts exactly
    /// where the replaced range ends. Edits outside the replaced section
    /// still apply.
    #[test]
    fn edits_to_sections_inside_a_replaced_one_overlap_it() {
        let replace = |body: &str, anchor: &str, content: &str| PagePatchEdit::ReplaceSection {
            anchor: anchor.into(),
            section_sha256: section_hashes(body)
                .0
                .into_iter()
                .find(|section| section.anchor == anchor)
                .unwrap()
                .sha256,
            content: content.into(),
        };
        let append = |anchor: &str| PagePatchEdit::Append {
            target: AppendTarget::Section,
            anchor: Some(anchor.into()),
            content: "x".into(),
        };

        // `### Child` is the last section inside `## One`.
        let body = "# T\n## One\nold\n### Child\nkept\n## Two\nother\n";
        let replace_one = replace(body, "One", "new");
        for batch in [
            [replace_one.clone(), append("Child")],
            [append("Child"), replace_one.clone()],
        ] {
            assert_eq!(apply(body, &batch), Err(PatchError::Overlapping));
        }
        // A nested section with no text of its own is replaced in place too.
        let empty_child = "# T\n## One\nold\n### Child\n## Two\nother\n";
        assert_eq!(
            apply(
                empty_child,
                &[
                    replace(empty_child, "One", "new"),
                    replace(empty_child, "Child", "y"),
                ]
            ),
            Err(PatchError::Overlapping)
        );

        // Controls: the next section and the page title are not inside it.
        assert_eq!(
            apply(body, &[replace_one.clone(), append("Two")]).unwrap(),
            "# T\n## One\nnew\n## Two\nother\nx\n"
        );
        assert_eq!(
            apply(body, &[replace_one, append("T")]).unwrap(),
            "# T\n## One\nnew\n## Two\nother\nx\n"
        );
        // When the replaced section ends the page, the end of the page and a
        // new section still follow it.
        let last = "# T\n## Zero\nz\n## One\nold\n### Child\nkept\n";
        let end_of_page = PagePatchEdit::Append {
            target: AppendTarget::EndOfPage,
            anchor: None,
            content: "footer".into(),
        };
        let next = PagePatchEdit::AddSection {
            anchor: "## Next".into(),
            content: "n".into(),
        };
        assert_eq!(
            apply(last, &[replace(last, "One", "new"), end_of_page, next]).unwrap(),
            "# T\n## Zero\nz\n## One\nnew\nfooter\n## Next\nn\n"
        );
        assert_eq!(
            apply(last, &[replace(last, "One", "new"), append("Child")]),
            Err(PatchError::Overlapping)
        );
    }

    /// The durable-pages skill tells agents how to write these edits, so every
    /// op it names has to be one the tool accepts. It once named
    /// `append_to_section`, which fails to deserialize.
    #[test]
    fn the_durable_pages_skill_names_only_ops_the_tool_accepts() {
        let skill = ai_memory_core::routing_skills::MANAGED_SKILLS
            .iter()
            .find(|skill| skill.name == "ai-memory-durable-pages")
            .expect("the durable-pages skill is bundled")
            .content;
        let mut named = skill
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|word| *word == "append" || word.ends_with("_section"))
            .collect::<Vec<_>>();
        named.sort_unstable();
        named.dedup();
        for op in ["append", "add_section", "replace_section"] {
            assert!(named.contains(&op), "the skill never names `{op}`");
        }
        for op in named {
            let edit = serde_json::json!({
                "op": op,
                "target": "section",
                "anchor": "## Notes",
                "section_sha256": "0",
                "content": "x",
            });
            assert!(
                serde_json::from_value::<PagePatchEdit>(edit).is_ok(),
                "the skill names `{op}`, which memory_patch_page rejects"
            );
        }
    }

    #[test]
    fn a_trailing_hash_closes_a_heading_only_after_a_space() {
        let body = "# T\n## Notes on C#\nold\n## Tail ##\nend\n";
        let anchors: Vec<_> = section_hashes(body)
            .0
            .into_iter()
            .map(|section| section.anchor)
            .collect();
        assert_eq!(anchors, ["T", "Notes on C#", "Tail"]);
        let patched = apply(
            body,
            &[
                PagePatchEdit::Append {
                    target: AppendTarget::Section,
                    anchor: Some("Notes on C#".into()),
                    content: "more".into(),
                },
                PagePatchEdit::AddSection {
                    anchor: "## F#".into(),
                    content: "new".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            patched,
            "# T\n## Notes on C#\nold\nmore\n## Tail ##\nend\n## F#\nnew\n"
        );
    }
}
