"""Tests for filter_test_alerts.py: python3 -I -m unittest discover -s .github/codeql"""

import tempfile
import unittest
from pathlib import Path

import filter_test_alerts as f

SOURCE = """\
use std::fmt;

pub fn log_session(session_id: &str) {
    eprintln!("session {session_id}");
}

#[cfg(test)]
mod tests {
    #[test]
    fn logs() {
        let session_id = "s";
        assert!(true, "{session_id}");
    }
}

pub fn after() {}
"""


def result(rule, uri, line):
    return {
        "ruleId": rule,
        "locations": [
            {
                "physicalLocation": {
                    "artifactLocation": {"uri": uri},
                    "region": {"startLine": line},
                }
            }
        ],
    }


class FilterTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        (root / "crates/x/src").mkdir(parents=True)
        (root / "crates/x/src/lib.rs").write_text(SOURCE, encoding="utf-8")
        self.code = f.TestCode(root)

    def tearDown(self):
        self.tmp.cleanup()

    def run_filter(self, *results):
        sarif = {"runs": [{"tool": {"driver": {"rules": []}}, "results": list(results)}]}
        dropped = f.filter_sarif(sarif, self.code)
        return [(r["ruleId"], f.location_of(r)[1]) for r in sarif["runs"][0]["results"]], dropped

    def test_inline_test_module_result_is_dropped(self):
        kept, dropped = self.run_filter(result("rust/cleartext-logging", "crates/x/src/lib.rs", 12))
        self.assertEqual(kept, [])
        self.assertEqual(len(dropped), 1)

    def test_production_code_result_is_kept(self):
        # Control: the same rule on the non-test function stays.
        kept, _ = self.run_filter(result("rust/cleartext-logging", "crates/x/src/lib.rs", 4))
        self.assertEqual(kept, [("rust/cleartext-logging", 4)])

    def test_code_after_the_test_module_is_kept(self):
        kept, _ = self.run_filter(result("rust/cleartext-logging", "crates/x/src/lib.rs", 16))
        self.assertEqual(kept, [("rust/cleartext-logging", 16)])

    def test_tests_directory_result_is_dropped(self):
        kept, _ = self.run_filter(
            result("rust/hard-coded-cryptographic-value", "crates/x/tests/suite/a.rs", 3)
        )
        self.assertEqual(kept, [])

    def test_other_rules_in_test_code_are_kept(self):
        kept, _ = self.run_filter(result("rust/sql-injection", "crates/x/tests/suite/a.rs", 3))
        self.assertEqual(kept, [("rust/sql-injection", 3)])

    def test_unreadable_source_keeps_its_result(self):
        kept, _ = self.run_filter(result("rust/cleartext-logging", "crates/x/src/missing.rs", 2))
        self.assertEqual(kept, [("rust/cleartext-logging", 2)])

    def test_rule_resolved_through_its_index(self):
        sarif = {
            "runs": [
                {
                    "tool": {"driver": {"rules": [{"id": "rust/cleartext-logging"}]}},
                    "results": [
                        {
                            "rule": {"index": 0},
                            "locations": result("", "crates/x/tests/a.rs", 1)["locations"],
                        }
                    ],
                }
            ]
        }
        f.filter_sarif(sarif, self.code)
        self.assertEqual(sarif["runs"][0]["results"], [])

    def test_module_ranges_skip_a_cfg_test_function(self):
        lines = ["#[cfg(test)]", "fn helper() {}", "#[cfg(test)]", "mod tests {", "    x", "}"]
        self.assertEqual(f.test_module_ranges(lines), [(4, 6)])

    def test_module_range_spans_a_fixture_with_a_column_zero_brace(self):
        # Regression for code-scanning #250: a raw-string fixture inside the test
        # module holds lines that are a bare `}` at column 0. The module must
        # still extend past them to its real closing brace, so a later test line
        # is recognised as test code.
        lines = [
            "#[cfg(test)]",
            "mod tests {",
            "    #[test]",
            "    fn writes_a_fixture() {",
            "        let script = r#\"",
            "try {",
            "  run();",
            "} finally {",
            "  server.close();",
            "}",  # column-0 brace inside the raw string
            "\"#;",
            "        let _ = script;",
            "    }",
            "    #[test]",
            "    fn logs_token() {",
            "        let token = \"aim_x\";",
            "        assert!(true, \"{token}\");",
            "    }",
            "}",
        ]
        ranges = f.test_module_ranges(lines)
        self.assertEqual(ranges, [(2, 19)])
        # The naive first-`}` scan stopped at line 10; the token assertion at 17
        # must now fall inside the module.
        self.assertTrue(any(start <= 17 <= end for start, end in ranges))

    def test_matching_brace_line_ignores_braces_in_strings_and_comments(self):
        lines = [
            "mod tests {",
            '    let a = "} not a close {";',
            "    // } not a close either",
            "    /* }",
            "       still a comment } */",
            "    let c = '}';",
            "}",
        ]
        self.assertEqual(f.matching_brace_line(lines, 0), 6)

    def test_inline_test_after_a_fixture_brace_is_dropped(self):
        # End-to-end: an alert on a test line past a column-0 fixture brace drops.
        root = Path(self.tmp.name)
        src = (
            "#[cfg(test)]\nmod tests {\n    fn fixture() {\n        let s = r#\"\n}\n\"#;\n"
            "        let _ = s;\n    }\n    fn logs(session_id: &str) {\n"
            '        eprintln!("{session_id}");\n    }\n}\n'
        )
        (root / "crates/x/src/lib.rs").write_text(src, encoding="utf-8")
        code = f.TestCode(root)
        self.assertTrue(code.contains("crates/x/src/lib.rs", 10))

    def test_out_of_line_module_paths_follow_rustc(self):
        from pathlib import PurePosixPath as P

        decls = [
            "#[cfg(test)]",
            '#[path = "equivalence.rs"]',
            "mod equivalence;",
            "#[cfg(test)]",
            "pub(crate) mod oracle;",
            "mod not_test;",
        ]
        self.assertEqual(
            f.out_of_line_test_modules(P("crates/x/src/router.rs"), decls),
            [
                ("crates/x/src/equivalence.rs", "crates/x/src/equivalence/"),
                ("crates/x/src/router/oracle.rs", "crates/x/src/router/oracle/"),
                ("crates/x/src/router/oracle/mod.rs", "crates/x/src/router/oracle/"),
            ],
        )
        suite = ["#[cfg(test)]", '#[path = "../tests/suite/mod.rs"]', "mod integration;"]
        self.assertEqual(
            f.out_of_line_test_modules(P("crates/x/src/lib.rs"), suite),
            [("crates/x/tests/suite/mod.rs", "crates/x/tests/suite/")],
        )

    def test_out_of_line_test_module_file_is_dropped(self):
        root = Path(self.tmp.name)
        (root / "crates/x/src/lib.rs").write_text(
            SOURCE + '#[cfg(test)]\n#[path = "oracle.rs"]\nmod oracle;\n', encoding="utf-8"
        )
        (root / "crates/x/src/oracle.rs").write_text("fn t() {}\n", encoding="utf-8")
        (root / "crates/x/src/live.rs").write_text("fn t() {}\n", encoding="utf-8")
        code = f.TestCode(root)
        self.assertTrue(code.contains("crates/x/src/oracle.rs", 1))
        self.assertFalse(code.contains("crates/x/src/live.rs", 1))


if __name__ == "__main__":
    unittest.main()
