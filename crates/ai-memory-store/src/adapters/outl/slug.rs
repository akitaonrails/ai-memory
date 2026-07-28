//! Deterministic, reversible mapping between the ai-memory page
//! identity 3-tuple `(workspace_id, project_id, path)` and an Outl page
//! slug.
//!
//! Outl slugs are single filename components (`is_valid_slug` rejects
//! `/`), so the slug is flat:
//!
//! ```text
//! <prefix>~<workspace_uuid>~<project_uuid>~<escaped path>
//! ```
//!
//! `~` never appears in a UUID, and the path escapes `%`, `~`, and `/`
//! percent-style so decoding is unambiguous. Using stable UUIDs (never
//! workspace/project *names*) keeps renames from mass-renaming pages.

use ai_memory_core::{PagePath, ProjectId, WorkspaceId};

const SEP: char = '~';
const UUID_LEN: usize = 36;

fn escape_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '~' => out.push_str("%7E"),
            '/' => out.push_str("%2F"),
            other => out.push(other),
        }
    }
    out
}

fn unescape_path(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let hi = chars.next()?;
        let lo = chars.next()?;
        let byte = u8::from_str_radix(&format!("{hi}{lo}"), 16).ok()?;
        out.push(byte as char);
    }
    Some(out)
}

/// Build the Outl slug for a page identity.
pub fn encode(
    prefix: &str,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    path: &PagePath,
) -> String {
    format!(
        "{prefix}{SEP}{workspace_id}{SEP}{project_id}{SEP}{}",
        escape_path(path.as_str())
    )
}

/// Slug prefix shared by every page of a project.
pub fn project_prefix(prefix: &str, workspace_id: WorkspaceId, project_id: ProjectId) -> String {
    format!("{prefix}{SEP}{workspace_id}{SEP}{project_id}{SEP}")
}

/// Slug prefix shared by every page of a workspace.
pub fn workspace_prefix(prefix: &str, workspace_id: WorkspaceId) -> String {
    format!("{prefix}{SEP}{workspace_id}{SEP}")
}

/// Decode a slug produced by [`encode`]. Returns `None` for slugs that
/// don't belong to ai-memory (different prefix or malformed shape) —
/// callers treat those pages as the user's own content and ignore them.
pub fn decode(prefix: &str, slug: &str) -> Option<(WorkspaceId, ProjectId, PagePath)> {
    let rest = slug.strip_prefix(prefix)?.strip_prefix(SEP)?;
    if rest.len() < UUID_LEN * 2 + 2 {
        return None;
    }
    let (ws_raw, rest) = rest.split_at(UUID_LEN);
    let rest = rest.strip_prefix(SEP)?;
    let (proj_raw, rest) = rest.split_at(UUID_LEN);
    let path_raw = rest.strip_prefix(SEP)?;

    let ws = WorkspaceId(uuid::Uuid::parse_str(ws_raw).ok()?);
    let proj = ProjectId(uuid::Uuid::parse_str(proj_raw).ok()?);
    let path = PagePath::new(unescape_path(path_raw)?).ok()?;
    Some((ws, proj, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_nested_paths() {
        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        for raw in [
            "notes/decisions/0001.md",
            "a~b/c%d.md",
            "simple.md",
            "with space/and~tilde%25.md",
        ] {
            let path = PagePath::new(raw).unwrap();
            let slug = encode("ai-memory", ws, proj, &path);
            assert!(outl_actions::is_valid_slug(&slug), "invalid slug: {slug}");
            let (dws, dproj, dpath) = decode("ai-memory", &slug).expect(raw);
            assert_eq!(dws, ws);
            assert_eq!(dproj, proj);
            assert_eq!(dpath.as_str(), raw);
        }
    }

    #[test]
    fn foreign_slugs_decode_to_none() {
        assert!(decode("ai-memory", "my-own-page").is_none());
        assert!(decode("ai-memory", "ai-memory~not-a-uuid~x~y").is_none());
    }

    #[test]
    fn prefixes_nest() {
        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        let path = PagePath::new("a.md").unwrap();
        let slug = encode("m", ws, proj, &path);
        assert!(slug.starts_with(&project_prefix("m", ws, proj)));
        assert!(slug.starts_with(&workspace_prefix("m", ws)));
    }
}
