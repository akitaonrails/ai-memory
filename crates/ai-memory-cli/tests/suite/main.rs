//! Single binary for this crate's integration tests.
//!
//! Every file in this directory is a module of this one test binary: one
//! link per rebuild instead of one per file. Cargo treats `tests/suite/main.rs`
//! as the single `suite` target and never builds the sibling files on their own,
//! so a new file must be declared here (`scripts/check-test-suites.*` enforces it).

mod auth_default_e2e;
mod auth_device_timeout;
mod autoscope_env;
mod backfill_dry_run;
mod backfill_e2e;
mod backfill_failures;
mod backfill_member;
mod backfill_oversized;
mod backup_agents;
mod backup_download;
mod completions;
mod credential_masking_e2e;
mod cross_origin_e2e;
mod doctor_e2e;
mod e2e_support;
mod external_capture;
mod external_capture_powershell;
mod external_capture_ts;
mod harness_switch_e2e;
mod hook_drain;
mod hook_payload;
mod install_script;
mod jail_toggles_e2e;
mod live_token_usage_e2e;
mod marker_scope;
mod mcp_bridge_stdio;
mod message_e2e;
mod opencode_dialect;
mod packaging;
mod pi_session_usage;
mod pi_tool_exposure;
mod plugins;
mod profile_e2e;
mod removal;
mod repo_layout;
mod routing_instructions;
mod routing_skills;
mod run_offline;
mod serve_config;
mod serve_shutdown;
mod server_profiles;
mod service_host_windows;
mod session_display_name;
mod setup_e2e;
mod shutdown_signals;
mod terminal_rendering;
mod upgrade_e2e;
mod windows_plain_paths;
mod workstream_provenance;
mod yolo_ai_jail;
mod yolo_kiro;

mod workstream_identity;
