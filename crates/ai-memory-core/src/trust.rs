//! Trust classification for observations and compiled pages (#2164).
//!
//! See `docs/design-memory-poisoning-provenance.md` (Steps 2–3).
//!
//! Every observation receives a [`TrustClass`] once at hook ingress from its
//! [`ObservationKind`]; it is never rewritten afterwards. A compiled page
//! derives its trust class from the least-trusted source observation recorded
//! in `page_sources` (V112), or inherits the superseded version's class on
//! citationless rewrites so an agent cannot launder a `tool_result` page into
//! `operator_authored` with a patch.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::observation::ObservationKind;

/// Origin trust class of an observation or wiki page (#2164).
///
/// Ordered strictly from least trusted to most trusted:
/// `ToolResult < ModelGenerated < OperatorAuthored`.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TrustClass {
    /// External content captured from a `post-tool-use` observation, or a page
    /// compiled from at least one such observation.
    ToolResult,
    /// Assistant-generated or harness-metadata observation, a page derived
    /// solely from such observations, or a legacy row predating V113.
    #[default]
    ModelGenerated,
    /// Direct operator input (`user-prompt` observation, an explicit
    /// `memory_write_page` without a `session_id`, or a hand-edited wiki file).
    OperatorAuthored,
}

impl TrustClass {
    /// Return the less trusted of two trust classes.
    ///
    /// Ordering: `ToolResult < ModelGenerated < OperatorAuthored`.
    #[must_use]
    pub const fn least_trusted(self, other: Self) -> Self {
        match (self, other) {
            (Self::ToolResult, _) | (_, Self::ToolResult) => Self::ToolResult,
            (Self::ModelGenerated, _) | (_, Self::ModelGenerated) => Self::ModelGenerated,
            (Self::OperatorAuthored, Self::OperatorAuthored) => Self::OperatorAuthored,
        }
    }

    /// Return the least-trusted class across an iterator of classes, or `None`
    /// if the iterator is empty.
    #[must_use]
    pub fn least_trusted_of(iter: impl IntoIterator<Item = Self>) -> Option<Self> {
        iter.into_iter().reduce(Self::least_trusted)
    }

    /// Assign the immutable ingress trust class for an observation kind.
    ///
    /// - `UserPrompt` -> [`Self::OperatorAuthored`]
    /// - `PostToolUse` -> [`Self::ToolResult`]
    /// - `Stop` and lifecycle/metadata kinds -> [`Self::ModelGenerated`]
    #[must_use]
    pub const fn from_observation_kind(kind: ObservationKind) -> Self {
        match kind {
            ObservationKind::UserPrompt => Self::OperatorAuthored,
            ObservationKind::PostToolUse => Self::ToolResult,
            ObservationKind::Stop
            | ObservationKind::SessionStart
            | ObservationKind::PreToolUse
            | ObservationKind::PreCompact
            | ObservationKind::PostCompaction
            | ObservationKind::Notification
            | ObservationKind::SessionEnd
            | ObservationKind::Other => Self::ModelGenerated,
        }
    }

    /// Canonical snake_case string for SQLite storage and JSON responses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OperatorAuthored => "operator_authored",
            Self::ModelGenerated => "model_generated",
            Self::ToolResult => "tool_result",
        }
    }
}

impl fmt::Display for TrustClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TrustClass {
    type Err = crate::MemoryError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "operator_authored" => Ok(Self::OperatorAuthored),
            "model_generated" => Ok(Self::ModelGenerated),
            "tool_result" => Ok(Self::ToolResult),
            other => Err(crate::MemoryError::MalformedRecord(format!(
                "unknown trust class: {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_class_ordering_and_least_trusted() {
        assert!(TrustClass::ToolResult < TrustClass::ModelGenerated);
        assert!(TrustClass::ModelGenerated < TrustClass::OperatorAuthored);
        assert!(TrustClass::ToolResult < TrustClass::OperatorAuthored);

        assert_eq!(
            TrustClass::least_trusted(TrustClass::OperatorAuthored, TrustClass::ToolResult),
            TrustClass::ToolResult
        );
        assert_eq!(
            TrustClass::least_trusted(TrustClass::OperatorAuthored, TrustClass::ModelGenerated),
            TrustClass::ModelGenerated
        );
        assert_eq!(
            TrustClass::least_trusted(TrustClass::ModelGenerated, TrustClass::ToolResult),
            TrustClass::ToolResult
        );
        assert_eq!(
            TrustClass::least_trusted(TrustClass::OperatorAuthored, TrustClass::OperatorAuthored),
            TrustClass::OperatorAuthored
        );
        assert_eq!(TrustClass::least_trusted_of(std::iter::empty()), None);
        assert_eq!(
            TrustClass::least_trusted_of([
                TrustClass::OperatorAuthored,
                TrustClass::ModelGenerated,
                TrustClass::ToolResult,
            ]),
            Some(TrustClass::ToolResult)
        );
    }

    #[test]
    fn trust_class_roundtrips_strings_and_json() {
        for (class, expected) in [
            (TrustClass::OperatorAuthored, "operator_authored"),
            (TrustClass::ModelGenerated, "model_generated"),
            (TrustClass::ToolResult, "tool_result"),
        ] {
            assert_eq!(class.as_str(), expected);
            assert_eq!(class.to_string(), expected);
            assert_eq!(expected.parse::<TrustClass>().unwrap(), class);
            assert_eq!(
                serde_json::to_string(&class).unwrap(),
                format!("\"{expected}\"")
            );
            assert_eq!(
                serde_json::from_str::<TrustClass>(&format!("\"{expected}\"")).unwrap(),
                class
            );
        }
        assert!("unknown".parse::<TrustClass>().is_err());
    }

    #[test]
    fn observation_kind_mapping_matches_design() {
        assert_eq!(
            TrustClass::from_observation_kind(ObservationKind::UserPrompt),
            TrustClass::OperatorAuthored
        );
        assert_eq!(
            TrustClass::from_observation_kind(ObservationKind::PostToolUse),
            TrustClass::ToolResult
        );
        for kind in [
            ObservationKind::Stop,
            ObservationKind::SessionStart,
            ObservationKind::PreToolUse,
            ObservationKind::PreCompact,
            ObservationKind::PostCompaction,
            ObservationKind::Notification,
            ObservationKind::SessionEnd,
            ObservationKind::Other,
        ] {
            assert_eq!(
                TrustClass::from_observation_kind(kind),
                TrustClass::ModelGenerated,
                "{kind:?}"
            );
        }
    }
}
