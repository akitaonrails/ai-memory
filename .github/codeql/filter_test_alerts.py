"""Drop CodeQL results that only fire on Rust test code before upload.

Three Rust queries match on names, not on data flow from a real secret:
`rust/cleartext-logging` and `rust/cleartext-transmission` treat any variable
called `session_id`, `secret`, `token` or `username` as sensitive, and
`rust/hard-coded-cryptographic-value` flags every literal key a test feeds a
cipher or a token check. In tests those names hold synthetic fixtures and the
"log" is an assertion message, so each new test re-opened alerts that were
dismissed one by one (more than 80 of them so far).

Results for those rules are removed only when they sit in test code: a file
under a `tests/` directory, the dev-only test-support crate, a line inside a
top-level `#[cfg(test)]` module, or the file (and submodules) of a top-level
`#[cfg(test)] mod name;` declaration. Every other query, and these three on
production code, reach code scanning unchanged. A source file that cannot be
read keeps its results.

Usage: python3 -I filter_test_alerts.py <root> <file.sarif>...
"""

import json
import re
import sys
from pathlib import Path

USAGE = "usage: python3 -I filter_test_alerts.py <root> <file.sarif>..."
FILTERED_RULES = frozenset(
    {
        "rust/cleartext-logging",
        "rust/cleartext-transmission",
        "rust/hard-coded-cryptographic-value",
    }
)
TEST_CRATE_PREFIX = "crates/ai-memory-test-support/"
# rustfmt puts a top-level item's header and closing brace at column 0.
TEST_MOD_HEADER = re.compile(r"^(pub(\([a-z]+\))? )?mod \w+ \{$")
TEST_MOD_DECL = re.compile(r"^(pub(\([a-z]+\))? )?mod (\w+);$")
PATH_ATTR = re.compile(r'^#\[path = "([^"]+)"\]$')
SOURCE_DIRS = ("crates", "companions", "evals")


def cfg_test_item(lines, index):
    """For a column-0 `#[cfg(test)]` at `index`, the index of the item it gates
    and the value of any `#[path = "..."]` attribute in between."""
    if lines[index] != "#[cfg(test)]":
        return None, None
    item = index + 1
    path_attr = None
    while item < len(lines) and lines[item].startswith("#["):
        match = PATH_ATTR.match(lines[item])
        if match:
            path_attr = match.group(1)
        item += 1
    return (item, path_attr) if item < len(lines) else (None, None)


def _raw_string_prefix(line, i):
    """If `line[i:]` opens a raw string (`r"`, `r#"`, `br#"`), return
    `(num_hashes, index_after_opening_quote)`; otherwise None. `line[i]` is the
    `r` or `b` and must begin a token, so a bare `r`/`b` inside an identifier
    (`for`, a variable named `b`) is not mistaken for a prefix."""
    if i > 0 and (line[i - 1].isalnum() or line[i - 1] == "_"):
        return None
    j = i + 1 if line[i] == "b" else i
    if j >= len(line) or line[j] != "r":
        return None
    j += 1
    hashes = 0
    while j < len(line) and line[j] == "#":
        hashes += 1
        j += 1
    if j < len(line) and line[j] == '"':
        return hashes, j + 1
    return None


def _skip_char_or_lifetime(line, i):
    """Advance past a char literal or lifetime at `line[i] == "'"` so a `'{'` or
    `'}'` byte is not counted as a brace."""
    length = len(line)
    if i + 1 < length and line[i + 1] == "\\":
        j = i + 2
        while j < length and line[j] != "'":
            j += 1
        return j + 1 if j < length else length
    if i + 2 < length and line[i + 2] == "'":
        return i + 3
    return i + 1


def matching_brace_line(lines, header):
    """0-based index of the line whose brace closes the item opened on
    `lines[header]` (its line ends in `{`). Rust strings, raw strings, char
    literals, and line/block comments are skipped, so a `}` inside a fixture
    string does not end the item early (rustfmt otherwise indents real body
    lines, leaving only the item's own brace at column 0). Falls back to the
    last line when the braces never balance."""
    depth = 0
    mode = "code"  # code | string | raw | block
    block_depth = 0
    raw_hashes = 0
    escape = False
    line_no = header
    while line_no < len(lines):
        line = lines[line_no]
        i = 0
        while i < len(line):
            c = line[i]
            if mode == "code":
                pair = line[i : i + 2]
                if c == '"':
                    mode, escape = "string", False
                elif pair == "//":
                    break
                elif pair == "/*":
                    mode, block_depth = "block", 1
                    i += 2
                    continue
                elif c == "'":
                    i = _skip_char_or_lifetime(line, i)
                    continue
                elif c in "rb" and (prefix := _raw_string_prefix(line, i)) is not None:
                    raw_hashes, i = prefix
                    mode = "raw"
                    continue
                elif c == "{":
                    depth += 1
                elif c == "}":
                    depth -= 1
                    if depth == 0:
                        return line_no
            elif mode == "string":
                if escape:
                    escape = False
                elif c == "\\":
                    escape = True
                elif c == '"':
                    mode = "code"
            elif mode == "raw":
                if c == '"' and line[i + 1 : i + 1 + raw_hashes] == "#" * raw_hashes:
                    mode = "code"
                    i += 1 + raw_hashes
                    continue
            elif mode == "block":
                pair = line[i : i + 2]
                if pair == "/*":
                    block_depth += 1
                    i += 2
                    continue
                if pair == "*/":
                    block_depth -= 1
                    i += 2
                    if block_depth == 0:
                        mode = "code"
                    continue
            i += 1
        line_no += 1
    return len(lines) - 1


def test_module_ranges(lines):
    """1-based inclusive line ranges of top-level `#[cfg(test)]` modules."""
    ranges = []
    index = 0
    while index < len(lines):
        header, _ = cfg_test_item(lines, index)
        if header is None or not TEST_MOD_HEADER.match(lines[header]):
            index += 1
            continue
        end = matching_brace_line(lines, header)
        ranges.append((header + 1, end + 1))
        index = end + 1
    return ranges


def out_of_line_test_modules(relative, lines):
    """Paths (relative to the root) a file's top-level `#[cfg(test)] mod x;` load.

    Each entry is a module file plus the directory its own submodules live
    in, following rustc's rules: `#[path]` is relative to the declaring file's
    directory; otherwise `lib.rs`, `main.rs` and `mod.rs` keep children beside
    themselves and any other `stem.rs` keeps them under `stem/`.
    """
    parent = relative.parent
    own_dir = parent if relative.name in ("lib.rs", "main.rs", "mod.rs") else parent / relative.stem
    found = []
    for index in range(len(lines)):
        decl, path_attr = cfg_test_item(lines, index)
        match = TEST_MOD_DECL.match(lines[decl]) if decl is not None else None
        if not match:
            continue
        if path_attr:
            candidates = [parent / path_attr]
        else:
            name = match.group(3)
            candidates = [own_dir / f"{name}.rs", own_dir / name / "mod.rs"]
        for module in candidates:
            sub_dir = module.parent if module.name == "mod.rs" else module.parent / module.stem
            found.append((normalize(module), normalize(sub_dir) + "/"))
    return found


def normalize(path):
    parts = []
    for part in path.parts:
        if part == "..":
            if parts:
                parts.pop()
        elif part != ".":
            parts.append(part)
    return "/".join(parts)


class TestCode:
    def __init__(self, root):
        self.root = Path(root)
        self.cache = {}
        self.test_files = None
        self.test_dirs = None

    def scan_out_of_line_modules(self):
        self.test_files, self.test_dirs = set(), set()
        for top in SOURCE_DIRS:
            for source in sorted((self.root / top).rglob("*.rs")):
                try:
                    lines = source.read_text(encoding="utf-8").splitlines()
                except (OSError, UnicodeDecodeError):
                    continue
                relative = source.relative_to(self.root)
                for module, sub_dir in out_of_line_test_modules(relative, lines):
                    self.test_files.add(module)
                    self.test_dirs.add(sub_dir)

    def contains(self, uri, line):
        if not uri.endswith(".rs"):
            return False
        if "/tests/" in f"/{uri}" or uri.startswith(TEST_CRATE_PREFIX):
            return True
        if self.test_files is None:
            self.scan_out_of_line_modules()
        if uri in self.test_files or any(uri.startswith(d) for d in self.test_dirs):
            return True
        if uri not in self.cache:
            try:
                text = (self.root / uri).read_text(encoding="utf-8")
            except (OSError, UnicodeDecodeError):
                self.cache[uri] = None
            else:
                self.cache[uri] = test_module_ranges(text.splitlines())
        ranges = self.cache[uri]
        if ranges is None or line is None:
            return False
        return any(start <= line <= end for start, end in ranges)


def location_of(result):
    try:
        physical = result["locations"][0]["physicalLocation"]
        uri = physical["artifactLocation"]["uri"]
    except (KeyError, IndexError, TypeError):
        return None, None
    return uri, physical.get("region", {}).get("startLine")


def rule_of(run, result):
    if "ruleId" in result:
        return result["ruleId"]
    try:
        index = result["rule"]["index"]
        return run["tool"]["driver"]["rules"][index]["id"]
    except (KeyError, IndexError, TypeError):
        return None


def filter_sarif(sarif, test_code):
    """Remove filtered test-code results in place; return what was dropped."""
    dropped = []
    for run in sarif.get("runs", []):
        kept = []
        for result in run.get("results", []):
            rule = rule_of(run, result)
            uri, line = location_of(result)
            if rule in FILTERED_RULES and uri and test_code.contains(uri, line):
                dropped.append(f"{rule} {uri}:{line}")
            else:
                kept.append(result)
        if "results" in run:
            run["results"] = kept
    return dropped


def main(argv):
    if len(argv) < 3:
        print(USAGE, file=sys.stderr)
        return 2
    test_code = TestCode(argv[1])
    for name in argv[2:]:
        path = Path(name)
        sarif = json.loads(path.read_text(encoding="utf-8"))
        dropped = filter_sarif(sarif, test_code)
        path.write_text(json.dumps(sarif), encoding="utf-8")
        print(f"{path}: dropped {len(dropped)} test-code result(s)")
        for entry in dropped:
            print(f"  {entry}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
