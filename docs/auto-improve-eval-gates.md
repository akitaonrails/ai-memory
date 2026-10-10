# Auto-Improve Eval Gates

`[auto_improve.eval]` lets operators guard high-impact auto-improvement
proposals with a small executable scorer. The scorer runs after LLM validation
and before staging or auto-approval. Hooks never run eval commands.

## Configuration

```toml
[auto_improve.eval]
enabled = true
command = "python3 docs/examples/auto-improve-eval/score_proposal.py"
timeout_secs = 30
targets = ["_rules", "procedures"]
min_delta = 0.0
```

The command is split on whitespace and executed directly, not through a shell.
Use a wrapper script if you need quoting, environment setup, or multiple
commands.

## Request contract

The scorer receives one JSON object on stdin:

```json
{
  "path": "procedures/release.md",
  "kind": "procedure",
  "operation": "update",
  "edit_mode": "patch",
  "title": "Release Procedure",
  "confidence": 0.91,
  "rationale": "Capture the repeated release checklist.",
  "before_body": "# Release Procedure\n\n## Steps\n- Run tests\n",
  "after_body": "# Release Procedure\n\n## Steps\n- Run tests\n- Run deploy smoke checks\n",
  "expected_base_body_sha256": "..."
}
```

`before_body` is empty for create proposals. `expected_base_body_sha256` is
present only for patch proposals that were materialized against a known base.

## Response contract

The scorer must print one JSON object to stdout:

```json
{ "score_before": 0.72, "score_after": 0.76, "passed": true }
```

Fields:

- `passed` is required. `false` rejects the targeted proposal.
- `score_before` and `score_after` are optional. When both are present,
  `score_after - score_before` must be at least `min_delta`.
- `reason` is optional and should explain a rejection in one short sentence.

Command errors, timeouts, invalid JSON, missing `passed`, `passed = false`, and
insufficient score delta all fail closed for the targeted proposal. Other
proposals in the same run can still proceed.

## Recorded results

The review report includes optional `eval_results`, one record for each targeted
proposal evaluated by the existing gate. The server generates each `eval_id` as a
UUID v7 for that scorer attempt, independently of the review run ID. Its status is
`success` when the response passes the existing criteria, `rejected` when a valid
response has `passed = false` or insufficient score delta, `failure` for command,
I/O, or response parsing errors, and `timeout` when the deadline expires.
Non-targeted proposals and disabled evals produce no records.

Each record contains the target path, `checker_name` (the program's basename),
`checker_invocation_digest` (SHA-256 of the configured command string), and
`proposal_sha256` (SHA-256 of the serialized JSON request sent on stdin).
`before_body_sha256` and `after_body_sha256` hash the exact UTF-8 bodies in that
request. Optional `materialized_base_body_sha256` identifies the server-resolved
base used to materialize a patch, which can differ from the current `before_body`
if the page changed before eval. These records do not certify a later page version.
The invocation digest does not hash executable or script contents and cannot
prove which checker code ran. No executable is read to produce these records.

Scores and `passed` come only from the scorer response. Reasons are scrubbed with
the built-in sanitizer before being capped at 2,048 characters. They remain
untrusted, attributed data. Extra scorer/model fields cannot supply eval IDs,
digests, or independent results; ordinary evidence quotes remain ordinary quotes.

Staging stores all results in the existing run config JSON. For an accepted
proposal, its eval result is also attached to the `auto_improve_eval` evidence
entry in the pending sidecar. The sidecar remains a snapshot at staging time,
without a new approval or execution step. Reports without `eval_results` and
older evidence entries without `eval_result` remain readable.
Deserializing a review report always discards `eval_results`, including forged
or malformed values. Only results held by the live server review can be attached
to pending evidence; reading a report cannot restore independent observations.

## Scorer design rules

- Keep scorers deterministic, fast, and side-effect-free.
- Read only stdin and local project files that are safe to inspect.
- Do not call LLMs, mutate files, run deploys, or depend on network services.
- Return bounded reasons; ai-memory caps captured eval evidence.
- Prefer simple checks that match the target path: heading structure for
  procedures, forbidden placeholders for `_rules`, or project-specific smoke
  assertions for critical docs.

## Examples

This repository includes two dependency-free templates:

- [`docs/examples/auto-improve-eval/score_proposal.py`](examples/auto-improve-eval/score_proposal.py):
  Python scorer that checks basic structure and placeholders.
- [`docs/examples/auto-improve-eval/score_proposal.sh`](examples/auto-improve-eval/score_proposal.sh):
  POSIX shell wrapper around an embedded Python scorer for hosts that prefer a
  script entrypoint.

Try them with the sample payload:

```bash
python3 docs/examples/auto-improve-eval/score_proposal.py \
  < docs/examples/auto-improve-eval/sample-proposal.json

sh docs/examples/auto-improve-eval/score_proposal.sh \
  < docs/examples/auto-improve-eval/sample-proposal.json
```

Both print compact JSON suitable for ai-memory's eval gate.
