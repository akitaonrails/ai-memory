# Auto-Improve Eval Example Scorers

These examples are intentionally small and deterministic. They read one
proposal JSON object from stdin and print one eval response JSON object to
stdout. `score_proposal.py` needs `python3`; `score_proposal.sh` applies the same
checks with only POSIX `sh` and `awk`, so it also runs inside the official Docker
image, which has no Python. Neither matches section names or wording, so pages
written in any language score the same way.

Use them as templates, not as universal quality gates. Real projects should
replace the checks with project-specific invariants that are safe to run before
an auto-improvement proposal is staged.

```toml
[auto_improve.eval]
enabled = true
command = "sh docs/examples/auto-improve-eval/score_proposal.sh"
timeout_secs = 30
targets = ["_rules", "procedures"]
min_delta = 0.0
```

Smoke test:

```bash
python3 docs/examples/auto-improve-eval/score_proposal.py < \
  docs/examples/auto-improve-eval/sample-proposal.json
sh docs/examples/auto-improve-eval/score_proposal.sh < \
  docs/examples/auto-improve-eval/sample-proposal.json
```

Both scorers must give the same verdict; `test_score_proposal.py` runs them on
the same proposals, including pages in Portuguese and Japanese:

```bash
python3 -m unittest discover -s docs/examples/auto-improve-eval -p 'test_*.py'
```
