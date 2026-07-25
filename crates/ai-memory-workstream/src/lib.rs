//! Read-only native harness adapters used by `ai-memory run`.

mod harness;
mod repository;
mod transcript;

pub use harness::{
    LaunchMode, LaunchPlan, ManagedHarness, allows_native_session_adoption, apply_yolo,
    build_launch_plan,
};
pub use repository::{RepositoryIdentity, inspect_repository};
pub use transcript::{
    ExportedTranscript, NativeSessionCandidate, discover_native_session, export_transcript,
    list_native_sessions, wait_for_transcript_flush,
};
