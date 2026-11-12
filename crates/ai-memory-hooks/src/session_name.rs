//! Optional harness-supplied label on an ordinary hook event.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::Path;

use ai_memory_core::{SanitizeConfig, Sanitizer, SessionDisplayName, truncate_utf8_bytes};
use serde::Deserialize;
use serde_json::{Value, json};

/// Names and their source revisions travel together through the spool.
pub const MARKER: &str = "_ai_memory_session_name";
const INDEX_TAIL_BYTES: u64 = 64 * 1024;
const MAX_WIRE_BYTES: usize = 4096;

/// Find this thread's latest complete entry in the bounded tail of the
/// [Codex session index](https://github.com/openai/codex/blob/067249a28329feec4a097f55413894acb4a6378a/codex-rs/rollout/src/session_index.rs).
#[must_use]
pub fn codex_marker(home: &Path, session_id: &str, privacy: &SanitizeConfig) -> Option<Value> {
    #[derive(Deserialize)]
    struct Entry<'a> {
        id: &'a str,
        thread_name: String,
        updated_at: &'a str,
    }

    uuid::Uuid::parse_str(session_id).ok()?;
    if !home.is_absolute() {
        return None;
    }
    let path = home.canonicalize().ok()?.join("session_index.jsonl");
    let mut file = crate::antigravity::open_regular_file(&path)?;
    let end = file.metadata().ok()?.len();
    let start = end.saturating_sub(INDEX_TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    file.take(end - start).read_to_end(&mut tail).ok()?;
    let mut lines = tail.split(|byte| *byte == b'\n');
    if start != 0 {
        lines.next();
    }
    // The final fragment may still be in flight. Codex terminates each entry.
    lines.next_back();
    let entry = lines.rev().find_map(|line| {
        serde_json::from_slice::<Entry<'_>>(line)
            .ok()
            .filter(|entry| entry.id == session_id)
    })?;
    let updated_at = crate::payload::bounded_hook_timestamp(entry.updated_at)?;
    let name = Sanitizer::new(privacy).ok()?.scrub(&entry.thread_name);
    let name = name.trim();
    if name.is_empty() || name.len() > MAX_WIRE_BYTES {
        return None;
    }
    Some(json!({ "name": name, "updated_at": updated_at }))
}

pub(crate) fn from_marker(raw: &Value, sanitizer: &Sanitizer) -> Option<SessionDisplayName> {
    let marker = raw.get(MARKER)?;
    let name = marker.get("name")?.as_str()?;
    if name.len() > MAX_WIRE_BYTES {
        return None;
    }
    let updated = crate::payload::bounded_hook_timestamp(marker.get("updated_at")?.as_str()?)?;
    let scrubbed = sanitizer.scrub(name);
    let name = scrubbed.trim();
    if name.is_empty() {
        return None;
    }
    Some(SessionDisplayName {
        name: truncate_utf8_bytes(name, 256),
        updated_ns: updated.as_nanosecond().try_into().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_display_name_reads_only_complete_records_in_the_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "aaaaaaaa-1111-4444-8888-000000000001";
        let row = json!({"id":id,"thread_name":"kept","updated_at":"2025-01-02T00:00:00Z"});
        let index = tmp.path().join("session_index.jsonl");
        let privacy = SanitizeConfig::default();
        std::fs::write(
            &index,
            format!("{row}\n{}\n", "x".repeat(INDEX_TAIL_BYTES as usize)),
        )
        .unwrap();
        assert!(codex_marker(tmp.path(), id, &privacy).is_none());
        std::fs::write(&index, format!("not json\n{row}\n{row}")).unwrap();
        assert_eq!(
            codex_marker(tmp.path(), id, &privacy).unwrap()["name"],
            "kept"
        );
        #[cfg(unix)]
        {
            std::fs::rename(&index, tmp.path().join("elsewhere")).unwrap();
            std::os::unix::fs::symlink("elsewhere", index).unwrap();
            assert!(codex_marker(tmp.path(), id, &privacy).is_none());
        }
    }

    #[test]
    fn session_display_name_requires_a_valid_revision_and_scrubs_before_clipping() {
        let sanitizer = Sanitizer::new(&SanitizeConfig {
            extra_patterns: vec!["PRIVATE_SENTINEL".into()],
            ..Default::default()
        })
        .unwrap();
        let marker = |name: &str, time: &str| json!({MARKER: {"name":name,"updated_at":time}});
        for (name, time) in [
            ("discard", "invalid"),
            ("discard", "2500-01-01T00:00:00Z"),
            ("  ", "2025-01-01T00:00:00Z"),
        ] {
            assert!(from_marker(&marker(name, time), &sanitizer).is_none());
        }
        let name = format!("{} PRIVATE_SENTINEL", "z".repeat(248));
        let safe = from_marker(&marker(&name, "2025-01-01T00:00:00Z"), &sanitizer).unwrap();
        assert!(safe.name.len() <= 256);
        assert!(!safe.name.contains("PRIV"));
    }
}
