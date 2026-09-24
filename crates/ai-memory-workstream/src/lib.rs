//! Read-only native harness adapters used by `ai-memory run`.

mod harness;
mod repository;
mod transcript;

pub use harness::{
    LaunchMode, LaunchPlan, LaunchRoots, ManagedHarness, allows_native_session_adoption,
    apply_yolo, build_launch_plan, build_launch_plan_with_env, clean_path, crush_data_dir,
    crush_global_config_path, env_dir_override, has_native_session_selector,
    kiro_explicit_session_id, kiro_selects_non_default_engine, kiro_selects_v2_engine,
    kiro_selects_v3_engine, omp_agent_dir, omp_profile_flag, omp_profile_flag_env,
    store_override_vars,
};
pub use repository::{RepositoryIdentity, inspect_repository};
pub use transcript::{
    AmbiguousNativeSession, ExportedTranscript, NativeSessionCandidate, discover_native_session,
    export_transcript, kiro_harness_from_source_cursor, kiro_v3_resume_uses_default_store,
    list_native_sessions, native_session_exists, native_session_in_checkout,
    wait_for_transcript_flush,
};
