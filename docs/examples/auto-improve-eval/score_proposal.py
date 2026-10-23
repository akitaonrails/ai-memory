#!/usr/bin/env python3
"""Example ai-memory auto-improve eval scorer.

Reads proposal JSON from stdin and emits:
  {"passed": bool, "score_before": float, "score_after": float, "reason": str?}

The checks are intentionally simple, deterministic, and independent of the
language the wiki is written in (section names and wording are never matched):
- procedures should have at least one `##` section and at least one list item
  for their steps;
- rules should carry text beyond their heading and stay short enough for an
  agent instruction;
- all targeted docs should have an H1 and avoid TODO/TBD placeholders (matched
  as whole upper-case words, so a word like "todo" in prose is not one).

`score_proposal.sh` applies the same checks with `sh` and `awk` only.
"""

from __future__ import annotations

import json
import re
import sys

LIST_ITEM = re.compile(r"^[ \t]*(?:[-*+]|[0-9]+[.)])[ \t]+\S", re.MULTILINE)
PLACEHOLDER = re.compile(r"(?<![A-Za-z0-9_])(?:TODO|TBD)(?![A-Za-z0-9_])")


def has_text_beyond_headings(body: str) -> bool:
    return any(
        line.strip(" \t") and not line.lstrip(" \t").startswith("#")
        for line in body.split("\n")
    )


def score_body(path: str, body: str) -> tuple[float, list[str]]:
    score = 0.0
    reasons: list[str] = []

    if re.search(r"^#[ \t]+\S", body, re.MULTILINE):
        score += 0.25
    else:
        reasons.append("missing H1 heading")

    if not PLACEHOLDER.search(body):
        score += 0.20
    else:
        reasons.append("contains TODO/TBD placeholder")

    if path.startswith("procedures/"):
        if re.search(r"^##[ \t]+\S", body, re.MULTILINE):
            score += 0.20
        else:
            reasons.append("procedure has no ## section")
        if LIST_ITEM.search(body):
            score += 0.25
        else:
            reasons.append("procedure has no list of steps")
    elif path.startswith("_rules/"):
        if has_text_beyond_headings(body):
            score += 0.35
        else:
            reasons.append("rule has no text beyond its heading")
        if len(body.strip(" \t\r\n")) <= 2400:
            score += 0.10
        else:
            reasons.append("rule is too long for an agent instruction")
    else:
        score += 0.45

    return min(score, 1.0), reasons


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError as exc:
        print(json.dumps({"passed": False, "reason": f"invalid input JSON: {exc}"}))
        return 0

    path = str(payload.get("path", ""))
    before_body = str(payload.get("before_body", ""))
    after_body = str(payload.get("after_body", ""))

    score_before, _ = score_body(path, before_body)
    score_after, reasons = score_body(path, after_body)
    passed = score_after >= 0.70 and score_after >= score_before

    response: dict[str, object] = {
        "passed": passed,
        "score_before": round(score_before, 4),
        "score_after": round(score_after, 4),
    }
    if not passed:
        response["reason"] = "; ".join(reasons) or "score did not improve"

    print(json.dumps(response, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
