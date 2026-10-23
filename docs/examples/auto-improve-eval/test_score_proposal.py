"""Tests for the example eval scorers: both give the same verdict, in any language.

Run with: python3 -m unittest discover -s docs/examples/auto-improve-eval -p 'test_*.py'
"""

from __future__ import annotations

import json
import subprocess
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCORERS = {
    "python": [sys.executable, "-I", str(HERE / "score_proposal.py")],
    "sh": ["sh", str(HERE / "score_proposal.sh")],
}
PT_PROCEDURE = (
    "# Procedimento de publicação\n\n## Objetivo\n"
    "Publicar uma versão verificada; todo o fluxo é revisado.\n\n"
    "## Passos\n- Rodar todos os testes.\n- Conferir o /admin/status.\n"
)


def proposal(path: str, after_body: str, before_body: str = "") -> bytes:
    sample = json.loads((HERE / "sample-proposal.json").read_text(encoding="utf-8"))
    sample.update(path=path, after_body=after_body, before_body=before_body)
    return json.dumps(sample, ensure_ascii=False).encode("utf-8")


def score(scorer: str, stdin: bytes) -> dict[str, object]:
    completed = subprocess.run(
        SCORERS[scorer], input=stdin, capture_output=True, check=True, timeout=30
    )
    return json.loads(completed.stdout)


class ScorerParityTest(unittest.TestCase):
    def assert_verdict(self, stdin: bytes, passed: bool) -> None:
        results = {name: score(name, stdin) for name in SCORERS}
        for name, result in results.items():
            self.assertIs(result["passed"], passed, f"{name}: {result}")
        python, shell = results["python"], results["sh"]
        for field in ("score_before", "score_after", "reason"):
            self.assertEqual(python.get(field), shell.get(field), field)

    def test_the_sample_proposal_passes(self) -> None:
        self.assert_verdict((HERE / "sample-proposal.json").read_bytes(), True)

    def test_a_portuguese_procedure_passes_and_todo_in_prose_is_no_placeholder(
        self,
    ) -> None:
        self.assert_verdict(proposal("procedures/publicacao.md", PT_PROCEDURE), True)

    def test_a_japanese_procedure_passes(self) -> None:
        body = "# リリース手順\n\n## 手順\n1. テストを実行する\n2) デプロイする\n"
        self.assert_verdict(proposal("procedures/release.md", body), True)

    def test_a_portuguese_rule_passes(self) -> None:
        body = "# Testes\n\nSempre rode a suíte completa antes de publicar.\n"
        self.assert_verdict(proposal("_rules/testes.md", body), True)

    def test_rule_length_counts_characters_not_bytes(self) -> None:
        body = "# Regra\n\n" + "ação " * 470 + "\n"
        self.assertEqual(
            score("sh", proposal("_rules/longa.md", body))["score_after"], 0.9
        )
        self.assert_verdict(proposal("_rules/longa.md", body), True)

    def test_a_procedure_without_a_list_of_steps_fails(self) -> None:
        body = "# Procedimento\n\n## Passos\nRodar os testes e publicar.\n"
        self.assert_verdict(proposal("procedures/x.md", body), False)

    def test_an_upper_case_placeholder_without_h1_fails(self) -> None:
        body = "Sem título\n\n## Passos\n- TODO: completar\n"
        self.assert_verdict(proposal("procedures/x.md", body), False)

    def test_a_rewrite_that_scores_lower_than_the_page_fails(self) -> None:
        body = "# P\n\n## S\nsem lista\n"
        self.assert_verdict(proposal("procedures/x.md", body, PT_PROCEDURE), False)

    def test_escaped_json_and_nested_keys_are_read_as_the_server_sends_them(
        self,
    ) -> None:
        sample = json.loads(proposal("_rules/a.md", '# Regra\n\nUse "x" \\ e\ttab.\n'))
        nested = {"meta": {"path": "procedures/other", "list": [1, {"a": "}"}]}}
        stdin = json.dumps({**nested, **sample}, ensure_ascii=True).encode()
        self.assert_verdict(stdin, True)

    def test_an_escaped_backslash_before_n_is_not_read_as_a_line_break(self) -> None:
        # Decoded as a line break, the "# x" after C:\n would be an H1 and pass.
        body = "Caminho C:\\n# x sem título\n"
        self.assert_verdict(proposal("_rules/a.md", body), False)

    def test_a_non_json_input_fails_closed_in_the_shell_scorer(self) -> None:
        result = score("sh", b'{"path": "procedures/x", "after_body": "# a\n')
        self.assertIs(result["passed"], False)


if __name__ == "__main__":
    unittest.main()
