"""Code metrics over committed main (py/hkpy/codemetrics.py; user, 2026-09-24)."""
from __future__ import annotations

import json
import subprocess
from datetime import datetime

from hkpy import codemetrics as C

RUST = """use std::io;
// a comment
/* block
   comment */
pub fn outer(x: u32) -> u32 {
    let s = "a { brace in a string";
    if x > 1 {
        x + 1
    } else {
        unsafe { x }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "needs a HackRF One (run with --ignored)"]
    fn hil() {}
    #[test]
    #[ignore]
    fn bare() {}
}
"""


def _repo(tmp_path):
    r = tmp_path / "repo"
    r.mkdir()
    files = {
        "crates/hk-x/src/lib.rs": RUST,
        "crates/hk-x/src/tests.rs": "fn t() {\n    assert!(true);\n}\n",
        "crates/hk-x/tests/it.rs": "#[test]\nfn it() {}\n",
        "py/hkpy/a.py": "# comment\nimport os\n\nx = 1  # TODO later\n",
        "py/tests/test_a.py": "def test_a():\n    assert True\n",
        "ui/src/view/a.ts": "// c\nexport const a = 1;\n",
        "ui/e2e/a.e2e.mjs": "run();\n",
        "ui/e2e/quarantine.json": "[]\n",
        "docs/x.rs": "fn not_counted() {}\n",
        "spikes/s/a.py": "print(1)\n",
        "Cargo.lock": "x\n",
    }
    for path, body in files.items():
        p = r / path
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(body)
    g = lambda *a: subprocess.run(["git", "-C", str(r), *a], check=True, capture_output=True)  # noqa: E731
    g("init", "-q", "-b", "main")
    g("-c", "user.email=t@t", "-c", "user.name=t", "add", "-A")
    g("-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "one")
    return r


def test_lines_product_vs_test_and_exclusions(tmp_path):
    r = _repo(tmp_path)
    files = {f["path"]: f for f in C.read_tree(str(r), "main")}
    assert "docs/x.rs" not in files and "spikes/s/a.py" not in files and "Cargo.lock" not in files
    lib = files["crates/hk-x/src/lib.rs"]
    assert (lib["product"], lib["test"]) == (9, 9)                  # comments out; #[cfg(test)] to EOF is test
    assert files["crates/hk-x/src/tests.rs"]["product"] == 0      # a test module file
    assert files["crates/hk-x/tests/it.rs"]["test"] == 2
    assert (files["py/hkpy/a.py"]["product"], files["py/tests/test_a.py"]["test"]) == (2, 2)
    assert files["ui/src/view/a.ts"]["area"] == "ui/src/view" and files["ui/e2e/a.e2e.mjs"]["test"] == 1
    t = C.lines_table(list(files.values()))
    assert t["total"]["product"] == 9 + 2 + 1 and {r["name"] for r in t["by_area"]} >= {"hk-x", "py/hkpy", "ui/e2e"}


def test_longest_fn_proxy_and_hygiene(tmp_path):
    r = _repo(tmp_path)
    files = C.read_tree(str(r), "main")
    fns = {f["fn"]: f for f in C.longest_fns(files)}
    assert fns["outer"]["lines"] == 8 and not fns["outer"]["test"]   # the brace in the string is ignored
    assert fns["hil"]["test"]
    h = C.hygiene(files, str(r), "main", str(tmp_path))
    assert h["unsafe"] == 1 and h["todo"] == 1 and h["quarantined"] == 0
    assert dict(h["ignored_by_reason"]) == {"needs a HackRF One (run with --ignored)": 1, "(no reason given)": 1}


def test_churn_junit_and_one_sample_a_day(tmp_path):
    r = _repo(tmp_path)
    now = datetime.now().timestamp()
    ch = C.churn(str(r), "main", now)
    assert ch["totals"]["24h"]["added"] > 0 and ch["hottest_7d"][0]["changed"] > 0
    run = tmp_path / "ops" / "junit" / "abc"
    run.mkdir(parents=True)
    (run / "02-test-default.xml").write_text(
        '<testsuites><testcase name="a" classname="hk-x" time="1.5"/><testcase name="b" classname="hk-x::it" time="2.0"/></testsuites>')
    j = C.junit_tests(str(tmp_path / "ops"))
    assert j["by_area"] == {"hk-x": {"tests": 2, "seconds": 3.5}}
    m = C.build(str(r), str(tmp_path / "ops"), now)
    assert m["tests"]["by_area"][0]["name"] == "hk-x" and m["tests"]["by_area"][0]["s_per_1k"]
    assert set(m["caveats"]) == {"lines", "churn", "tests", "outliers", "hygiene", "trends"}
    C.build(str(r), str(tmp_path / "ops"), now + 60)                  # same day: no second sample
    rows = [json.loads(x) for x in (tmp_path / "ops" / "metrics.jsonl").read_text().splitlines()]
    assert len(rows) == 1 and rows[0]["product"] == m["lines"]["total"]["product"]


def test_clippy_warnings_come_from_the_last_lint_section(tmp_path):
    (tmp_path / "merge-runner.log").write_text(
        "gate: running just lint\nwarning: old\ngate: just lint took 20s (exit 0)\n"
        "gate: running just lint\nwarning: unused import\nwarning: `hk-x` (lib) generated 1 warning\n"
        "gate: just lint took 25s (exit 0)\n")
    w = C.clippy_warnings(str(tmp_path))
    assert w["warnings"] == 1 and w["line"].startswith("gate: just lint took 25s")
