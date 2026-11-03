#!/usr/bin/env python3
"""Prove focused Rust tests reject semantic faults; restore sources in finally.

Run alone, with no concurrent builds. This touches only clean tracked source
files, backs up their exact bytes, and never writes production data. A compiler
error, a failed baseline or a timeout does not count as a killed mutation.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
TARGET = ROOT / "target"
TARGET.mkdir(exist_ok=True)
OUT = Path(tempfile.mkdtemp(prefix="dashboard-core-mutations-", dir=TARGET))
LOCK = TARGET / "dashboard-core-mutations.lock"
REPORT = {"mutants": [], "complete": False}

CASES = [
    {'name': 'expose-foreign-page-citations', 'file': 'crates/ai-memory-store/src/provenance.rs', 'start': 'pub async fn page_provenance(', 'end': '\n}', 'before': 'AND s.workspace_id=?2 AND s.project_id=?3', 'after': 'AND ?2 IS NOT NULL AND ?3 IS NOT NULL', 'command': ['cargo', 'test', '-p', 'ai-memory-web', '--lib', 'contextual_page_sources_preserve_shared_pages_and_refuse_foreign_or_private_evidence']},
    {'name': 'ignore-response-time-lower-bound', 'file': 'crates/ai-memory-store/src/reader.rs', 'start': 'pub async fn token_usage_report_page(', 'end': '/// Total reported token usage', 'before': 'COALESCE(c.response_at,c.observed_at)>=?13', 'after': 'COALESCE(c.response_at,c.observed_at)>=COALESCE(c.response_at,c.observed_at)', 'command': ['cargo', 'test', '-p', 'ai-memory-store', '--lib', 'token_investigation_filters_before_paging_and_keeps_native_context']},
    {'name': 'expose-private-page-contradictions', 'file': 'crates/ai-memory-store/src/provenance.rs', 'start': 'pub async fn page_provenance(', 'end': '\n}', 'before': 'readable_repository_predicate("pg.project_id", viewer)', 'after': 'readable_repository_predicate("pg.project_id", viewer.and(None))', 'command': ['cargo', 'test', '-p', 'ai-memory-web', '--lib', 'contextual_page_sources_preserve_shared_pages_and_refuse_foreign_or_private_evidence']},
    {'name': 'expose-other-owners-page-sources', 'file': 'crates/ai-memory-store/src/provenance.rs', 'start': 'pub async fn page_provenance(', 'end': '\n}', 'before': 'OwnerFilter::User(a) => (1, a)', 'after': 'OwnerFilter::User(a) => (0, a)', 'command': ['cargo', 'test', '-p', 'ai-memory-web', '--lib', 'contextual_page_sources_preserve_shared_pages_and_refuse_foreign_or_private_evidence']},
    {'name': 'expose-admin-feature-settings', 'file': 'crates/ai-memory-web/src/features.rs', 'start': 'pub(crate) async fn states(', 'end': '\n}', 'before': 'if private && !admin', 'after': 'if private && false && !admin', 'command': ['cargo', 'test', '-p', 'ai-memory-web', '--lib', 'contextual_states_report_effective_configuration_and_mask_admin_settings']},
    {'name': 'ignore-exact-model-filter', 'file': 'crates/ai-memory-store/src/reader.rs', 'start': 'pub async fn token_usage_report_page(', 'end': '/// Total reported token usage', 'before': "COALESCE(c.model,'Not reported')=?10", 'after': "COALESCE(c.model,'Not reported')=COALESCE(c.model,'Not reported')", 'command': ['cargo', 'test', '-p', 'ai-memory-store', '--lib', 'token_investigation_filters_before_paging_and_keeps_native_context']},
    {
        "name": "rewind-live-response-cursor", "file": "crates/ai-memory-cli/src/commands/session_usage.rs",
        "start": "pub fn extract_live_calls(", "end": "/// Cumulative token usage",
        "before": "if cursor.offset > len", "after": "if true || cursor.offset > len",
        "command": ["cargo", "test", "-p", "ai-memory-cli", "--lib", "live_token_cursor_replays_until_commit_then_recovers_every_response"],
    },
    {
        "name": "advance-usage-before-enqueue", "file": "crates/ai-memory-cli/src/commands/hook.rs",
        "start": "        if !antigravity_turn_only {", "end": "        if let Some(prompts) = antigravity_prompts {",
        "before": "if hook_spool::enqueue_with_config(&spool, &entry, Some(&spool_policy)).is_err()",
        "after": "if { if let Some(batch) = live_batch.take() { batch.commit(); } hook_spool::enqueue_with_config(&spool, &entry, Some(&spool_policy)) }.is_err()",
        "command": ["cargo", "test", "-p", "ai-memory-cli", "--test", "suite", "native_hook_recovers_a_response_backlog_and_does_not_advance_on_spool_failure"],
    },
    {
        "name": "ignore-response-search", "file": "crates/ai-memory-store/src/reader.rs",
        "start": "pub async fn token_usage_report_page(", "end": "/// Total reported token usage",
        "before": "(?6 = '' OR instr", "after": "(?6 = ?6 OR instr",
        "command": ["cargo", "test", "-p", "ai-memory-store", "--lib", "token_call_queries_search_and_order_before_pagination_with_scope_and_owner_guards"],
    },
    {
        "name": "expose-other-owners-response-search", "file": "crates/ai-memory-store/src/reader.rs",
        "start": "pub async fn token_usage_report_page(", "end": "/// Total reported token usage",
        "before": "OwnerFilter::User(a) => (1, a)", "after": "OwnerFilter::User(a) => (0, a)",
        "command": ["cargo", "test", "-p", "ai-memory-store", "--lib", "token_call_queries_search_and_order_before_pagination_with_scope_and_owner_guards"],
    },

    {
        "name": "expose-private-notification-projects", "file": "crates/ai-memory-web/src/routes/notifications.rs",
        "start": "pub(crate) async fn handler(", "end": "let notifications =",
        "before": "pending_inbox_counts(viewer.map(|v| v.0.user()))", "after": "pending_inbox_counts(viewer.filter(|_| false).map(|v| v.0.user()))",
        "command": ["cargo", "test", "-p", "ai-memory-web", "--lib", "operations_notifications_are_read_only_and_hide_restricted_projects"],
    },
    {
        "name": "ignore-message-state-before-pagination", "file": "crates/ai-memory-store/src/dashboard.rs",
        "start": "pub async fn message_history_filtered(", "end": "/// Read this session",
        "before": "(?5 IS NULL OR state=?5)", "after": "(?5 IS NULL OR ?5 IS NOT NULL)",
        "command": ["cargo", "test", "-p", "ai-memory-web", "--lib", "operations_pending_mail_is_filtered_before_history_pagination"],
    },
    {
        "name": "hide-resumed-session-activity", "file": "crates/ai-memory-store/src/reader.rs",
        "start": "fn sessions_for_scope_sql(", "end": "fn handoff_listing_sql(",
        "before": "AND resumed.created_at > s.ended_at", "after": "AND resumed.created_at > s.ended_at AND 0",
        "command": ["cargo", "test", "-p", "ai-memory-store", "--lib", "resumed_summary_is_open_and_preserves_the_persisted_end"],
    },
    {
        "name": "expose-restricted-mailbox-projects", "file": "crates/ai-memory-web/src/routes/inbox.rs",
        "start": "pub(crate) async fn handler(", "end": "#[derive(Deserialize)]",
        "before": "list_projects_with_stats(viewer)", "after": "list_projects_with_stats(viewer.and(None))",
        "command": ["cargo", "test", "-p", "ai-memory-web", "--lib", "operations_inbox_queue_context_hides_foreign_projects_and_preserves_mail"],
    },
    {
        "name": "capture-only-at-session-end", "file": "crates/ai-memory-cli/src/commands/hook.rs",
        "start": 'if matches!(\n        args.event.as_str(),',
        "end": '    // Token-usage visibility',
        "before": '"post-tool-use" | "stop" | "pre-compact" | "session-end"', "after": '"session-end"',
        "command": ["cargo", "test", "-p", "ai-memory-cli", "--test", "suite", "native_hook_reports_live_usage_for_every_supported_event_and_harness"],
    },
    {
        "name": "count-codex-cache-as-fresh-input", "file": "crates/ai-memory-cli/src/commands/session_usage.rs",
        "start": "pub fn extract_live_calls(", "end": "/// Cumulative token usage",
        "before": "input.saturating_sub(cached)", "after": "input",
        "command": ["cargo", "test", "-p", "ai-memory-cli", "--lib", "live_token_codex_reads_modern_usage_without_counting_cached_input_twice"],
    },
    {
        "name": "let-replayed-counters-decrease", "file": "crates/ai-memory-store/src/ops.rs",
        "start": "pub fn upsert_token_calls(", "end": "\npub fn ",
        "before": "output_tokens = MAX(output_tokens, excluded.output_tokens)", "after": "output_tokens = excluded.output_tokens",
        "command": ["cargo", "test", "-p", "ai-memory-store", "--lib", "token_calls_keep_partial_reports_monotonic_in_rows_and_aggregates"],
    },
    {
        "name": "allow-the-wrong-harness-to-claim", "file": "crates/ai-memory-store/src/ops.rs",
        "start": "pub(crate) fn pop_message_in_transaction(", "end": "/// Cancel",
        "replacements": [("(to_agent IS NULL OR to_agent = ?3)", "(?3 IS NOT NULL)", 2), ("(to_agent IS NULL OR to_agent = ?4)", "(?4 IS NOT NULL)", 1)],
        "command": ["cargo", "test", "-p", "ai-memory-store", "--lib", "addressed_messages_are_claimed_once_only_by_the_selected_harness"],
    },
    {
        "name": "edit-mail-with-read-only-mailbox", "file": "crates/ai-memory-web/src/routes/inbox.rs",
        "start": "async fn editable_message(", "end": "/// Take a queued message",
        "before": "&input.project,\n        writer.viewer,\n        ProjectAccess::Write",
        "after": "&input.project,\n        writer.viewer,\n        ProjectAccess::Read",
        "command": ["cargo", "test", "-p", "ai-memory-web", "--lib", "operations_edit_needs_write_access_to_the_sender_project_too"],
    },
    {
        "name": "edit-mail-with-read-only-sender", "file": "crates/ai-memory-web/src/routes/inbox.rs",
        "start": "async fn editable_message(", "end": "/// Take a queued message",
        "before": "&origin.project_name,\n        writer.viewer,\n        ProjectAccess::Write",
        "after": "&origin.project_name,\n        writer.viewer,\n        ProjectAccess::Read",
        "command": ["cargo", "test", "-p", "ai-memory-web", "--lib", "operations_edit_needs_write_access_to_the_sender_project_too"],
    },
    {
        "name": "hold-foreign-mail-for-editing", "file": "crates/ai-memory-store/src/ops.rs",
        "start": "pub fn begin_message_edit(", "end": "/// Put an `editing` message",
        "before": "to_workspace_id = ?2 AND to_project_id = ?3",
        "after": "?2 IS NOT NULL AND ?3 IS NOT NULL",
        "command": ["cargo", "test", "-p", "ai-memory-store", "--lib", "a_message_being_edited_leaves_the_queue_until_it_is_saved"],
    },
    {
        "name": "finish-editing-foreign-mail", "file": "crates/ai-memory-store/src/ops.rs",
        "start": "pub fn finish_message_edit(", "end": "fn observation_kind_as_str(",
        "replacements": [("to_workspace_id = ?2 AND to_project_id = ?3", "?2 IS NOT NULL AND ?3 IS NOT NULL", 2)],
        "command": ["cargo", "test", "-p", "ai-memory-store", "--lib", "a_message_being_edited_leaves_the_queue_until_it_is_saved"],
    },
    {
        "name": "edit-collected-or-cancelled-mail", "file": "crates/ai-memory-store/src/ops.rs",
        "start": "pub fn begin_message_edit(", "end": "/// Put an `editing` message",
        "before": "state = 'pending'", "after": "state IN ('pending', 'claimed', 'cancelled')",
        "command": ["cargo", "test", "-p", "ai-memory-store", "--lib", "collected_or_cancelled_mail_cannot_be_taken_for_editing"],
    },
    {
        "name": "finish-editing-unheld-mail", "file": "crates/ai-memory-store/src/ops.rs",
        "start": "pub fn finish_message_edit(", "end": "fn observation_kind_as_str(",
        "replacements": [("state = 'editing'", "state IN ('editing', 'pending')", 2)],
        "command": ["cargo", "test", "-p", "ai-memory-store", "--lib", "a_message_being_edited_leaves_the_queue_until_it_is_saved"],
    },
    {
        "name": "claim-mail-held-for-editing", "file": "crates/ai-memory-store/src/ops.rs",
        "start": "pub(crate) fn pop_message_in_transaction(", "end": "/// Cancel",
        "replacements": [("state = 'pending'", "state IN ('pending', 'editing')", 3)],
        "command": ["cargo", "test", "-p", "ai-memory-store", "--lib", "a_message_being_edited_leaves_the_queue_until_it_is_saved"],
    },
]

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--case", choices=[case["name"] for case in CASES])
selected = parser.parse_args().case
if selected:
    CASES = [case for case in CASES if case["name"] == selected]


def run(command, log_name):
    log = OUT / (log_name + ".log")
    with log.open("w") as output:
        result = subprocess.run(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT, timeout=300)
    return result.returncode, log.read_text(errors="replace")


def replace_bytes(path, contents):
    staged = path.with_name(path.name + ".mutation-tmp")
    try:
        staged.write_bytes(contents)
        os.chmod(staged, path.stat().st_mode & 0o777)
        os.replace(staged, path)
    finally:
        staged.unlink(missing_ok=True)


with LOCK.open("x"):
    try:
        # The source is the same for every baseline, so each distinct test
        # command runs once, before any file is touched.
        for command in {tuple(case["command"]): case["command"] for case in CASES}.values():
            code, _ = run(command, command[-1] + "-baseline")
            assert code == 0, command[-1] + ": baseline failed"
        for case in CASES:
            path = ROOT / case["file"]
            subprocess.run(["git", "diff", "--exit-code", "HEAD", "--", case["file"]], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
            original = path.read_bytes()
            (OUT / (case["name"] + ".original")).write_bytes(original)
            text = original.decode()
            start = text.index(case["start"])
            end = text.index(case["end"], start)
            section = text[start:end]
            for before, after, count in case.get("replacements", [(case.get("before"), case.get("after"), 1)]):
                assert section.count(before) == count, case["name"] + ": source anchor drifted"
                section = section.replace(before, after)
            changed = (text[:start] + section + text[end:]).encode()
            try:
                replace_bytes(path, changed)
                code, output = run(case["command"], case["name"] + "-mutant")
                assert code != 0 and "test result: FAILED." in output, case["name"] + ": mutation survived or failed outside a test: " + output[-1500:]
                assert case["command"][-1] + " ... FAILED" in output, case["name"] + ": unrelated test failed"
                REPORT["mutants"].append({"name": case["name"], "killed": True, "test": case["command"][-1]})
                print("KILLED " + case["name"], flush=True)
            finally:
                assert path.read_bytes() == changed, "Source changed concurrently; original backup: " + str(OUT)
                replace_bytes(path, original)
        # Every changed file must be back to its exact committed content.
        subprocess.run(["git", "diff", "--exit-code", "HEAD", "--", *sorted({c["file"] for c in CASES})], cwd=ROOT, check=True)
    except Exception as error:
        REPORT["error"] = str(error)
        raise
    finally:
        try:
            # Source restoration alone leaves the last deliberately broken
            # executable in target/. Rebuild before another test consumes it.
            subprocess.run(["git", "diff", "--exit-code", "HEAD", "--", *sorted({c["file"] for c in CASES})], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
            with (OUT / "restore-build.log").open("w") as log:
                subprocess.run(["cargo", "build", "--locked", "--bin", "ai-memory"], cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, timeout=300, check=True)
            REPORT["executable_restored"] = True
            REPORT["complete"] = len(REPORT["mutants"]) == len(CASES) and "error" not in REPORT
        except Exception as error:
            REPORT["restore_error"] = str(error)
            raise
        finally:
            (OUT / "report.json").write_text(json.dumps(REPORT, indent=2) + "\n")
            LOCK.unlink(missing_ok=True)
            print("Core mutation report: " + str(OUT / "report.json"), flush=True)
