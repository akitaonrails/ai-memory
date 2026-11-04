//! The reserved `_ai_memory_file` hook field (#1843): the checkout-relative
//! path of the one file a kept file-tool PostToolUse named, or why the client
//! sent none. Versioned and always removed from the raw payload, like
//! `_ai_memory_assistant`. A separate top-level field rather than a key inside
//! `_ai_memory_capture`, whose `deny_unknown_fields` would make an older server
//! reject every claim.

use serde::Deserialize;
use serde_json::{Value, json};

/// Top-level payload key.
pub const FILE_FIELD_KEY: &str = "_ai_memory_file";
const FILE_FIELD_VERSION: u64 = 1;
const SKIP_REASON_MAX_CHARS: usize = 32;

/// Why a client sent no path for a call that named one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSkip {
    /// The file is outside the event's checkout.
    OutsideCheckout,
    /// The event's cwd has no checkout root.
    NoCheckout,
    /// The normalized path failed [`ai_memory_core::valid_file_path_key`].
    Refused,
}

impl FileSkip {
    const fn as_str(self) -> &'static str {
        match self {
            Self::OutsideCheckout => "outside_checkout",
            Self::NoCheckout => "no_checkout",
            Self::Refused => "refused",
        }
    }

    fn parse(reason: &str) -> Option<Self> {
        [Self::OutsideCheckout, Self::NoCheckout, Self::Refused]
            .into_iter()
            .find(|skip| skip.as_str() == reason)
    }
}

/// The field a client attaches: the path, or the reason it has none.
#[must_use]
pub fn file_field(path: Result<&str, FileSkip>) -> Value {
    match path {
        Ok(path) => json!({ "version": FILE_FIELD_VERSION, "path": path }),
        Err(skip) => json!({ "version": FILE_FIELD_VERSION, "skipped": skip.as_str() }),
    }
}

/// What the server read from the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileClaim {
    /// No field: an older or unsupported client.
    Absent,
    /// A version this server does not read.
    UnknownVersion,
    /// Not the shape [`file_field`] writes.
    Malformed,
    /// The client's skip reason; `None` for a well-formed reason this server
    /// does not know, so a newer client can add one without a version bump.
    Skipped(Option<FileSkip>),
    /// The path the client sent, not yet checked.
    Path(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFileField {
    version: u64,
    path: Option<String>,
    skipped: Option<String>,
}

/// Remove the field from `body` and read it. Removed whatever it holds, before
/// any gate, so it never reaches storage as raw text.
pub(crate) fn take_file_claim(body: &mut Value) -> FileClaim {
    let Some(value) = body
        .as_object_mut()
        .and_then(|object| object.remove(FILE_FIELD_KEY))
    else {
        return FileClaim::Absent;
    };
    if value
        .get("version")
        .and_then(Value::as_u64)
        .is_some_and(|version| version != FILE_FIELD_VERSION)
    {
        return FileClaim::UnknownVersion;
    }
    let Ok(field) = serde_json::from_value::<WireFileField>(value) else {
        return FileClaim::Malformed;
    };
    match (field.version, field.path, field.skipped) {
        (FILE_FIELD_VERSION, Some(path), None) => FileClaim::Path(path),
        (FILE_FIELD_VERSION, None, Some(reason))
            if !reason.is_empty()
                && reason.len() <= SKIP_REASON_MAX_CHARS
                && reason.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
        {
            FileClaim::Skipped(FileSkip::parse(&reason))
        }
        _ => FileClaim::Malformed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(field: Value) -> (FileClaim, Value) {
        let mut body = json!({ "tool_name": "Read", FILE_FIELD_KEY: field });
        let claim = take_file_claim(&mut body);
        (claim, body)
    }

    #[test]
    fn the_field_round_trips_and_is_always_removed() {
        let (claim, body) = take(file_field(Ok("src/lib.rs")));
        assert_eq!(claim, FileClaim::Path("src/lib.rs".into()));
        assert!(body.get(FILE_FIELD_KEY).is_none());

        for skip in [
            FileSkip::OutsideCheckout,
            FileSkip::NoCheckout,
            FileSkip::Refused,
        ] {
            assert_eq!(
                take(file_field(Err(skip))).0,
                FileClaim::Skipped(Some(skip))
            );
        }

        for malformed in [
            json!("src/lib.rs"),
            json!({ "version": 1 }),
            json!({ "version": 1, "path": "a", "skipped": "refused" }),
            json!({ "version": 1, "path": "a", "extra": true }),
            json!({ "version": 1, "skipped": "Not-A-Token" }),
            json!({ "version": 1, "skipped": "x".repeat(33) }),
            json!({ "version": 1, "skipped": "" }),
        ] {
            let (claim, body) = take(malformed.clone());
            assert_eq!(claim, FileClaim::Malformed, "{malformed}");
            assert!(body.get(FILE_FIELD_KEY).is_none(), "{malformed}");
        }

        let (claim, body) = take(json!({ "version": 2, "path": "a", "new_key": 1 }));
        assert_eq!(claim, FileClaim::UnknownVersion);
        assert!(body.get(FILE_FIELD_KEY).is_none());
    }

    /// The reason set is additive: a reason a newer client adds reaches this
    /// server as `other`, not as a malformed field.
    #[test]
    fn an_unknown_skip_reason_is_other_not_malformed() {
        let (claim, _) = take(json!({ "version": 1, "skipped": "symlinked_cwd" }));
        assert_eq!(claim, FileClaim::Skipped(None));
    }

    #[test]
    fn a_body_without_the_field_is_absent_and_untouched() {
        let mut body = json!({ "tool_name": "Read" });
        assert_eq!(take_file_claim(&mut body), FileClaim::Absent);
        assert_eq!(body, json!({ "tool_name": "Read" }));
    }
}
