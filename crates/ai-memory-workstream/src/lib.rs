//! Read-only native harness adapters used by `ai-memory run`.

mod harness;
mod jail;
mod repository;
mod transcript;

pub use harness::{
    LaunchMode, LaunchPlan, LaunchRoots, ManagedHarness, OpenCodeDialect, TranscriptCapability,
    allows_native_session_adoption, apply_claude_true_yolo, apply_yolo, build_launch_plan,
    build_launch_plan_with_env, build_launch_plan_with_env_lookup, clean_path, crush_data_dir,
    crush_global_config_path, env_dir_override, has_native_session_selector,
    is_interactive_session_invocation, kiro_explicit_session_id, kiro_selects_non_default_engine,
    kiro_selects_v2_engine, kiro_selects_v3_engine, omp_agent_dir, omp_profile_flag,
    omp_profile_flag_env, store_override_vars,
};
pub use jail::{
    FORWARDED_ENV_NAMES, JAIL_TOGGLES, JailChecklistItem, JailEnv, JailHostFacts, JailOs,
    JailSupport, JailToggle, JailToggleChoice, JailToggleError, JailToggleKind, ai_jail_support,
    build_ai_jail_invocation, checked_choices, current_jail_os, inside_ai_jail,
    inside_ai_jail_here, jail_checklist, jail_toggle, marked_choices, parse_jail_toggles,
    usable_ai_jail, usable_ai_jail_here,
};
pub use repository::{
    RepositoryFingerprints, RepositoryIdentity, inspect_repository, inspect_repository_fingerprints,
};
pub use transcript::{
    AmbiguousNativeSession, ClaudeContinuation, ExportedTranscript, NativeSessionCandidate,
    claude_continued_session, claude_live_background_attach_id, claude_session_ran_in_background,
    discover_native_session, export_transcript, export_transcript_delta, export_transcript_range,
    kiro_harness_from_source_cursor, kiro_v3_resume_uses_default_store, list_native_sessions,
    native_memory_dir, native_session_exists, native_session_in_checkout, native_store_root,
    transcript_baseline, transcript_interval_digests, wait_for_transcript_flush,
};
