//! Read-only native harness adapters used by `ai-memory run`.

mod harness;
mod jail;
mod jail_contract;
mod repository;
mod transcript;

pub use harness::{
    LaunchMode, LaunchPlan, LaunchRoots, ManagedHarness, OpenCodeDialect, TranscriptCapability,
    allows_native_session_adoption, apply_claude_true_yolo, apply_jail_process_isolation,
    apply_yolo, build_launch_plan, build_launch_plan_with_env, build_launch_plan_with_env_lookup,
    clean_path, crush_data_dir, crush_global_config_path, env_dir_override,
    has_native_session_selector, is_interactive_session_invocation, kiro_explicit_session_id,
    kiro_selects_non_default_engine, kiro_selects_v2_engine, kiro_selects_v3_engine, omp_agent_dir,
    omp_profile_flag, omp_profile_flag_env, store_override_vars,
};
pub use jail::{
    FORWARDED_ENV_NAMES, JAIL_TOGGLES, JailChecklistItem, JailEnv, JailHostFacts, JailOs, JailRisk,
    JailSupport, JailToggle, JailToggleChoice, JailToggleError, JailToggleKind, ai_jail_support,
    build_ai_jail_invocation, checked_choices, checklist_reason, current_jail_os, inside_ai_jail,
    inside_ai_jail_here, jail_checklist, jail_spec, jail_toggle, marked_choices,
    parse_jail_toggles, usable_ai_jail, usable_ai_jail_here,
};
pub use jail_contract::{
    JailContract, JailRefusal, JailStatus, ServerReach, ai_jail_contract, ai_jail_harness_name,
    contract_flags, contract_invocation, contract_status_invocation, harness_env_names,
    parse_capabilities, parse_status, server_reach, status_refusal,
};
pub use repository::{
    RepositoryFingerprints, RepositoryIdentity, inspect_repository, inspect_repository_fingerprints,
};
pub use transcript::{
    AmbiguousNativeSession, ClaudeContinuation, ExportedTranscript, NativeSessionCandidate,
    OVERSIZED_RECORD_LOSS, RECORD_CWD_KEY, SKIPPED_RECORD_KEY, claude_continued_session,
    claude_continued_sessions, claude_live_background_attach_id, claude_project_dir_name,
    claude_session_ran_in_background, discover_native_session, export_transcript,
    export_transcript_delta, export_transcript_range, find_native_session,
    kiro_harness_from_source_cursor, kiro_v3_resume_uses_default_store, list_native_sessions,
    native_memory_dir, native_session_exists, native_session_in_checkout, native_store_root,
    transcript_baseline, transcript_interval_digests, wait_for_transcript_flush,
};
