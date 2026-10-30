//! This crate's integration tests. Every file here is a module of the lib's
//! test harness (see the `integration` module in `src/lib.rs`), so they cost
//! no extra binary; a new file must be declared below.

mod access_breadth;
mod access_mode;
mod agent_messages;
mod audit_contamination;
mod audit_log;
mod auto_improve_staging;
mod belief_authority;
mod cjk_search;
mod client_activity;
mod fts_drift_status;
mod fts_stopwords_config;
mod handoff_ownership;
mod identity_resolution;
mod idle_session_end;
mod live_token_usage;
mod most_recently_active_scope;
mod multi_session;
mod observation_prune;
mod page_first_created;
mod pinned_pages;
mod profile;
mod project_authz;
mod project_link_graph;
mod reader_pool_bounds;
mod reclaim_ledger_versions;
mod related_walk;
mod retrieval_observation_stream;
mod retrieval_superseded;
mod retrieval_tuning_streams;
mod session_activity;
mod session_consolidation_status;
mod session_ids_touching_scope;
mod session_move;
mod session_observations;
mod session_scope_from_observations;
mod sessions_by_agent;
mod slot_visibility;
mod stress_writer_throughput;
mod wal_bounds;
mod workstream_provenance;
mod writer_immediate;
