"""/metrics phase 2 part B (py/hkpy/codecomplex.py; user, 2026-09-24)."""
from __future__ import annotations

import os
import subprocess
import time

import pytest

from hkpy import codecomplex as X
from hkpy import codemetrics as C

REAL = os.path.expanduser("~/.hackriff-ops/tools/bin/rust-code-analysis-cli")


def _repo(tmp_path):
    r = tmp_path / "r"
    src = r / "crates/hk-x/src"
    src.mkdir(parents=True)
    (src / "lib.rs").write_text(
        "pub fn simple() -> u32 { 1 }\n"
        "pub fn branchy(a: u32, b: u32) -> u32 {\n    if a > 1 {\n        if b > 2 { for _ in 0..a { if a == b { return 3; } } }\n    }\n    0\n}\n"
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { if true { if true { assert!(true) } } }\n}\n")
    g = lambda *a: subprocess.run(["git", "-C", str(r), "-c", "user.email=t@t", "-c", "user.name=t", *a], check=True, capture_output=True)  # noqa: E731
    g("init", "-q", "-b", "main")
    g("add", "-A")
    g("commit", "-qm", "one")
    return r


def test_missing_tool_is_named_not_guessed(tmp_path, monkeypatch):
    monkeypatch.setattr(X.shutil, "which", lambda name: None)
    r = X.analyse([], str(tmp_path), "main", str(tmp_path), time.time())
    assert r["error"].startswith("rust-code-analysis-cli not installed")


def test_stream_functions_and_percentiles():
    objs = list(X._decode_stream('{"a": 1}\n{"b": {"c": [1]}}  {"d": 2}'))
    assert objs == [{"a": 1}, {"b": {"c": [1]}}, {"d": 2}]
    fns = []
    X._functions({"spaces": [{"kind": "impl", "spaces": [{"kind": "function", "name": "f", "start_line": 3, "end_line": 9,
                                                        "metrics": {"cognitive": {"sum": 7}, "cyclomatic": {"sum": 4}}, "spaces": []}]}]}, fns)
    assert [(f["name"], f["cognitive"], f["cyclomatic"]) for f in fns] == [("f", 7, 4)]
    assert X._pct([1, 2, 3, 4, 10], .9) == 10 and X._pct([], .5) is None


def test_hotspots_rank_churn_times_complexity_and_skip_tests(tmp_path, monkeypatch):
    r = _repo(tmp_path)
    files = C.read_tree(str(r), "main")
    monkeypatch.setattr(X, "tool", lambda ops: "/bin/true")
    monkeypatch.setattr(X, "run", lambda exe, repo, sha, paths: {
        "crates/hk-x/src/lib.rs": {"cyclomatic": 9, "cognitive": 12, "effort": 1, "mi": 50.0, "sloc": 14,
                                   "functions": [{"name": "branchy", "start": 2, "end": 7, "cognitive": 11, "cyclomatic": 6, "effort": 1, "mi": 60},
                                                 {"name": "t", "start": 11, "end": 11, "cognitive": 3, "cyclomatic": 3, "effort": 1, "mi": 90}]}})
    a = X.analyse(files, str(r), "main", str(tmp_path), time.time())
    assert [w["name"] for w in a["worst"]] == ["branchy"]                  # the test fn (after #[cfg(test)]) is not product
    assert a["hotspots"][0]["path"] == "crates/hk-x/src/lib.rs" and a["hotspots"][0]["score"] == a["hotspots"][0]["churn_30d"] * 12
    assert a["areas"][0]["name"] == "hk-x" and a["areas"][0]["functions"] == 1


@pytest.mark.skipif(not os.path.exists(REAL), reason="rust-code-analysis-cli not installed here")
def test_the_real_tool_on_a_tiny_repo(tmp_path):
    r = _repo(tmp_path)
    res = X.run(REAL, str(r), "main", ["crates/hk-x/src/lib.rs"])
    fns = {f["name"]: f for f in res["crates/hk-x/src/lib.rs"]["functions"]}
    assert fns["branchy"]["cognitive"] > fns["simple"]["cognitive"] == 0
