"""The experiment ledger enforces one-at-a-time, a prepared rollback, and guard vetoes."""

import argparse
import json
from datetime import datetime

from hkpy import experiment


def _args(**over):
    base = dict(id="E-1", hypothesis="overlap raises landings/h", knob=["WORK_GATE_ALONE=0"],
                baseline="2026-09-23 00:00..2026-09-23 06:00", metric="landings_per_h_24h",
                guard=["real_reds_24h <= baseline*1.25", "blocked_minutes < 30"], gates=6, hours=8.0,
                rule="keep if +30%", rollback="just knobs set WORK_GATE_ALONE=1")
    base.update(over)
    return argparse.Namespace(**base)


def _ops(tmp_path):
    (tmp_path / "merge-runner.log").write_text("", encoding="utf-8")
    (tmp_path / "work-runner.log").write_text("", encoding="utf-8")
    (tmp_path / "docs").mkdir()
    (tmp_path / "docs" / "ops-experiments.md").write_text("# ledger\n", encoding="utf-8")
    return str(tmp_path), str(tmp_path)


def test_guard_grammar():
    g = experiment.parse_guard("real_reds_24h <= baseline*1.25")
    assert g["metric"] == "real_reds_24h" and g["op"] == "<=" and g["factor"] == 1.25 and g["value"] is None
    g = experiment.parse_guard("blocked_minutes < 30")
    assert g["value"] == 30.0 and g["factor"] is None
    try:
        experiment.parse_guard("nonsense")
        raise AssertionError("accepted a bad guard")
    except ValueError:
        pass


def test_new_writes_both_ledgers_and_refuses_a_second_open(tmp_path):
    ops, root = _ops(tmp_path)
    now = datetime(2026, 9, 23, 13, 30)
    assert experiment.cmd_new(ops, root, _args(), now=now) == 0
    rec = json.loads((tmp_path / "experiments.jsonl").read_text().splitlines()[-1])
    assert rec["event"] == "open" and rec["id"] == "E-1" and "baseline" in rec and rec["rollback"]
    md = (tmp_path / "docs" / "ops-experiments.md").read_text()
    assert "## E-1" in md and "**Rollback:**" in md
    assert experiment.cmd_new(ops, root, _args(id="E-2"), now=now) == 3          # one at a time


def test_new_requires_a_rollback_and_understood_guards(tmp_path):
    ops, root = _ops(tmp_path)
    assert experiment.cmd_new(ops, root, _args(rollback="  ")) == 2
    assert experiment.cmd_new(ops, root, _args(guard=["??"])) == 2
    assert experiment.current(ops) is None


def test_close_refuses_keep_when_a_guard_is_broken_and_records_otherwise(tmp_path):
    ops, root = _ops(tmp_path)
    t0 = datetime(2026, 9, 23, 13, 30)
    assert experiment.cmd_new(ops, root, _args(guard=["blocked_minutes < 10"]), now=t0) == 0
    # a 20-minute hold inside the experiment window breaks the guard
    (tmp_path / "hold.jsonl").write_text(json.dumps({
        "ts": datetime(2026, 9, 23, 14, 0).timestamp(), "event": "hold", "minutes": 20,
        "until": datetime(2026, 9, 23, 14, 20).timestamp(), "why": "x"}) + "\n", encoding="utf-8")
    later = datetime(2026, 9, 23, 16, 0)
    assert experiment.cmd_close(ops, root, "keep", "looked good", now=later) == 3
    assert experiment.current(ops) is not None
    assert experiment.cmd_close(ops, root, "rollback", "guard broke", now=later) == 0
    rec = json.loads((tmp_path / "experiments.jsonl").read_text().splitlines()[-1])
    assert rec["event"] == "close" and rec["decision"] == "rollback" and rec["blocked_minutes"] == 20.0
    assert experiment.current(ops) is None
    assert "E-1 closed" in (tmp_path / "docs" / "ops-experiments.md").read_text()


def test_blocked_minutes_counts_cap_floor_periods(tmp_path):
    ops, _ = _ops(tmp_path)
    t = lambda h, m=0: datetime(2026, 9, 23, h, m).timestamp()  # noqa: E731
    (tmp_path / "env.jsonl").write_text(
        json.dumps({"ts": t(10), "set": {"WORK_CAP": "1"}}) + "\n" + json.dumps({"ts": t(10, 45), "set": {"WORK_CAP": "4"}}) + "\n",
        encoding="utf-8")
    assert experiment.blocked_minutes(ops, datetime(2026, 9, 23, 9), datetime(2026, 9, 23, 12)) == 45.0


def test_status_and_list_do_not_crash_on_an_empty_ledger(tmp_path, capsys):
    ops, _ = _ops(tmp_path)
    assert experiment.cmd_status(ops) == 0 and experiment.cmd_list(ops) == 0
    assert "none open" in capsys.readouterr().out
