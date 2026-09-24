"""Why tickets get handed back for a fix run (py/hkpy/fixes.py; user, 2026-09-24).

The user asked for the reason class on the runner's FIX line, one record per fix run, a /worklog
table with a per-day tally and the tally in the digest. The traps: the runner writes no attention
line for an ordinary review fail (only at escalation), and review.json is overwritten by the next
review - so a backfill that reads only the attention files calls every past fix OTHER (it did:
11 of 11 on 2026-09-23 before `backfill` read the work runner's own log).
"""
from __future__ import annotations

import importlib.util
import json
import pathlib
import sys
from datetime import datetime

import pytest

from hkpy import fixes

_WR = pathlib.Path(__file__).resolve().parents[2] / "ops" / "work-runner.py"


@pytest.mark.parametrize("line,cls", [
    ("REVIEW_FAIL VERDICT: FAIL x.rs:3 - off by one (full review: /r.json)", "REVIEW_FAIL"),
    ("UNCOMMITTED 2 modified files left uncommitted in /wt", "UNCOMMITTED"),
    ("09-23 15:26  task-t613  T-613  CONFLICT(skipped from bulk)", "CONFLICT"),
    ("09-23 23:01  task-t801  T-801  GATE_FAIL", "GATE_FAIL"),
    ("held fix", "OTHER"),
    ("", "OTHER"),
])
def test_classify(line, cls):
    assert fixes.classify(line) == cls


def test_review_reason_drops_the_prefix():
    cls, why = fixes.reason_for("REVIEW_FAIL VERDICT: FAIL justfile:283 - omits the target (full review: /r.json)")
    assert cls == "REVIEW_FAIL" and why.startswith("justfile:283 - omits")


MLOG = """[09-23 22:40:00] BULK attempt: task-t801 task-t2
[09-23 22:58:10] TRIAGE: re-running the failing tests alone: hk-api::routes::serves_tiles
[09-23 22:59:40] TRIAGE: hk-api::routes::serves_tiles FAILS alone - a real defect
[09-23 23:01:00] GATE FAILED task-t801 (isolated)
"""


def test_gate_fail_reason_is_what_triage_found_red():
    cls, why = fixes.reason_for("09-23 23:01  task-t801  T-801  GATE_FAIL", MLOG, "task-t801")
    assert (cls, why) == ("GATE_FAIL", "hk-api::routes::serves_tiles (fails alone)")


def test_gate_fail_with_no_red_test_says_lint_build():
    log = "[09-23 10:00:00] MERGE start task-t9\n[09-23 10:05:00] TRIAGE: no FAIL lines found\n[09-23 10:05:01] GATE FAILED task-t9 x\n"
    assert fixes.gate_fail_detail(log, "task-t9") == "lint/build/ui-unit (no test line red)"
    assert fixes.reason_for("x  task-t7  T-7  GATE_FAIL", log, "task-t7")[1] == "merge gate red"


def _ts(s):
    return int(datetime(2026, 9, 23, *map(int, s.split(":"))).timestamp())


@pytest.fixture
def ops(tmp_path):
    (tmp_path / "work" / "T-801").mkdir(parents=True)
    (tmp_path / "work" / "T-801" / "fix2.json").write_text(json.dumps({"result": "## I scoped the scroll lock to Explore.\nmore"}))
    (tmp_path / "work-runner.log").write_text("\n".join([
        "[09-23 19:19:27] REVIEW T-801 pid=8868 (bounded)",
        "[09-23 19:20:32] FIX T-801 attempt 1: resumed session a75b5bb5 pid=1 (bounded)",
        "[09-23 19:21:38] REVIEW T-801 pid=17204 (bounded)",
        "[09-23 19:22:00] tick: 1 running ['T-801']",
        "[09-23 19:23:14] FIX T-801 attempt 2: resumed session a75b5bb5 pid=2 (bounded)",
        "[09-23 01:13:21] FIX T-513: hold cleared - relaunching the held fix (UNCOMMITTED 1 modified files left uncommitted in /wt)",
        "[09-23 01:13:21] FIX T-513 attempt 2: resumed session 2b1537 pid=3 (bounded)",
    ]) + "\n")
    (tmp_path / "merge-needs-attention.txt").write_text("09-23 15:18  task-t700  T-700  GATE_FAIL\n")
    done = [
        {"ticket": "T-801", "branch": "task-t801", "kind": "fix", "started": _ts("19:23:14"), "outcome": "done-to-review", "minutes": 0.6},
        {"ticket": "T-513", "branch": "task-t513", "kind": "fix", "started": _ts("01:13:21"), "outcome": "done-to-review", "minutes": 7.6},
        {"ticket": "T-700", "branch": "task-t700", "kind": "fix", "started": _ts("15:19:03"), "outcome": "done", "minutes": 1.9},
        {"ticket": "T-9", "branch": "task-t9", "kind": "fix", "started": _ts("20:00:00"), "outcome": "done", "minutes": 3,
         "attempt": 1, "reason_class": "CONFLICT", "reason": "no longer merges cleanly into main"},
        {"ticket": "T-5", "branch": "task-t5", "kind": "work", "started": _ts("20:00:00"), "outcome": "done"},
    ]
    (tmp_path / "work-done.jsonl").write_text("".join(json.dumps(o) + "\n" for o in done))
    (tmp_path / "work-claims.json").write_text(json.dumps({"T-4": {
        "ticket": "T-4", "kind": "fix", "state": "running", "started": _ts("21:00:00"), "fix_attempts": 1,
        "fix_reason_class": "REVIEW_FAIL", "fix_reason": "x.rs:1 wrong"}}))
    return tmp_path


def test_rows_backfill_from_the_work_runner_log_not_only_attention(ops):
    by = {r["ticket"]: r for r in fixes.rows(str(ops), 0)}
    assert set(by) == {"T-801", "T-513", "T-700", "T-9", "T-4"}                   # the work run is not a fix
    # a bare REVIEW before the FIX: the review failed; its verdict is gone, the fix says what it fixed
    assert by["T-801"]["reason_class"] == "REVIEW_FAIL" and by["T-801"]["attempt"] == 2
    assert by["T-801"]["reason"] == "fixed: I scoped the scroll lock to Explore." and by["T-801"]["backfilled"]
    # a relaunched held fix carries its own fail line
    assert (by["T-513"]["reason_class"], by["T-513"]["reason"]) == ("UNCOMMITTED", "1 modified files left uncommitted in /wt")
    # no work-runner line: the merge runner's attention line
    assert by["T-700"]["reason_class"] == "GATE_FAIL"
    # recorded by the runner: taken as is; a running fix from its claim
    assert (by["T-9"]["reason_class"], by["T-9"]["backfilled"]) == ("CONFLICT", False)
    assert (by["T-4"]["outcome"], by["T-4"]["reason"]) == ("running", "x.rs:1 wrong")


def test_tally_per_day_and_line(ops):
    rs = fixes.rows(str(ops), 0)
    t = fixes.tally(rs)
    assert t == {"2026-09-23": {"REVIEW_FAIL": 2, "UNCOMMITTED": 1, "GATE_FAIL": 1, "CONFLICT": 1}}
    assert fixes.tally_line(rs).startswith("fix runs 24h: REVIEW_FAIL 2 · ")
    assert fixes.tally_line([]) == "fix runs 24h: none"
    s = fixes.summary(str(ops), now=_ts("22:00:00"))
    assert len(s["rows"]) == 5 and s["line"].startswith("fix runs 24h: REVIEW_FAIL 2")


def _runner(tmp_path, monkeypatch):
    spec = importlib.util.spec_from_file_location("hk_work_runner_fx", _WR)
    R = importlib.util.module_from_spec(spec)
    sys.modules["hk_work_runner_fx"] = R
    spec.loader.exec_module(R)
    monkeypatch.setattr(R, "S", str(tmp_path))
    monkeypatch.setattr(R, "WORKDIR", str(tmp_path / "work"))
    monkeypatch.setattr(R, "DONE", str(tmp_path / "work-done.jsonl"))
    monkeypatch.setattr(R, "MERGE_LOG", str(tmp_path / "merge-runner.log"))
    monkeypatch.setattr(R, "gate_holds_dispatch", lambda: False)
    return R


def test_runner_logs_the_class_and_records_it(tmp_path, monkeypatch):
    R = _runner(tmp_path, monkeypatch)
    (tmp_path / "merge-runner.log").write_text(MLOG)
    (tmp_path / "work" / "T-801").mkdir(parents=True)
    logged = []
    monkeypatch.setattr(R, "log", logged.append)

    class P:
        pid = 42
        stdin = type("I", (), {"write": lambda self, s: None, "close": lambda self: None})()
    monkeypatch.setattr(R.subprocess, "Popen", lambda *a, **k: P())
    c = {"ticket": "T-801", "branch": "task-t801", "wt": str(tmp_path), "session_id": "a75b5bb5xx", "kind": "work",
         "state": "queued", "started": 0}
    c = R.launch_fix(c, "09-23 23:01  task-t801  T-801  GATE_FAIL")
    assert logged[-1].startswith("FIX T-801 attempt 1 [GATE_FAIL] hk-api::routes::serves_tiles (fails alone): resumed session a75b5bb5")
    assert (c["fix_reason_class"], c["kind"], c["fix_attempts"]) == ("GATE_FAIL", "fix", 1)
    R.record_done(c, "done-to-review", {})
    rec = json.loads((tmp_path / "work-done.jsonl").read_text())
    assert (rec["reason_class"], rec["reason"], rec["attempt"]) == ("GATE_FAIL", "hk-api::routes::serves_tiles (fails alone)", 1)
    # a work run carries no reason fields
    R.record_done(dict(c, kind="work"), "done", {})
    assert "reason_class" not in json.loads((tmp_path / "work-done.jsonl").read_text().splitlines()[1])


def test_a_review_fix_names_the_finding_and_the_log_caps_it_at_120(tmp_path, monkeypatch):
    R = _runner(tmp_path, monkeypatch)
    seen = {}
    monkeypatch.setattr(R, "_run_fix", lambda c, n, prompt: seen.update(c=c) or c)
    R.launch_fix({"ticket": "T-1", "branch": "task-t1", "wt": "/tmp", "session_id": "s", "kind": "work", "started": 0},
                 "REVIEW_FAIL VERDICT: FAIL " + "x" * 300 + " (full review: /r.json)")
    assert seen["c"]["fix_reason_class"] == "REVIEW_FAIL" and seen["c"]["fix_reason"] == "x" * 200
