"""The dashboard's /flow.json builder (ops/monitor.py:build_flow_panel), a pure function of
`ops` and `now` — mirrors the fixture style of tests/test_flow.py. Imported via importlib from
its file path (ops/monitor.py is not itself a package); importing it must have no side effects
(no threads started, no server bound) since the module-level thread starters live under
`if __name__ == "__main__"`.
"""

import importlib.util
import json
import os
import sys
from datetime import datetime

import pytest

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
MONITOR_PATH = os.path.join(REPO, "ops", "monitor.py")


def _load_monitor():
    spec = importlib.util.spec_from_file_location("hk_ops_monitor_under_test", MONITOR_PATH)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


@pytest.fixture(scope="module")
def monitor():
    before = set(sys.modules)
    m = _load_monitor()
    yield m
    # importing exercises _flow_modules(), which inserts REPO/py into sys.path and imports
    # hkpy.* — leave those alone (harmless, other tests may want them), but don't leak the
    # dynamically-loaded monitor module itself under a name other tests might collide with.
    for k in set(sys.modules) - before:
        if k == "hk_ops_monitor_under_test":
            sys.modules.pop(k, None)


LOG = """\
[09-23 05:13:37] === merge-runner up (DRY_RUN=0, bulk mode); watching q ===
[09-23 05:58:46] BULK attempt (1): task-t607
[09-23 05:58:50] BULK gate (just gate --base abc over 1 merged branch; may take 15-25 min)…
gate: just lint took 23s (exit 0)
gate: just test took 1797s (exit 0)
gate: just acceptance-ci took 568s (exit 0)
[09-23 06:40:00] BULK MERGED ✓ T-607
[09-23 07:00:00] MERGE start task-t846 (T-846, 3 commits ahead)
[09-23 07:00:01] GATE task-t846 (just gate-merge; may take 15-25 min)…
gate: just test-ui took 7s (exit 0)
[09-23 07:05:00] GATE FAILED task-t846 (attempt 1/2, tip deadbeef) -> abort + flag for AI
"""

WORK = """\
[09-23 05:00:00] DISPATCH T-607 [opus/high] pid=1 -> /wt/t607
[09-23 06:30:00] VERSION: matches HEAD:ops/work-runner.py  ops=/x cap=6 dry=False
"""


def _ops(tmp_path, extra_hours_back=0):
    """A minimal ops dir the flow module can read from. The log timestamps are fixed at
    2026-09-23; `now` in the tests is chosen to sit just after them."""
    (tmp_path / "merge-runner.log").write_text(LOG, encoding="utf-8")
    (tmp_path / "work-runner.log").write_text(WORK, encoding="utf-8")
    t = lambda s: datetime(2026, 9, 23, *map(int, s.split(":"))).timestamp()  # noqa: E731
    (tmp_path / "handbacks.jsonl").write_text(
        json.dumps({"ts": t("05:50"), "ticket": "T-607", "outcome": "done"}) + "\n", encoding="utf-8")
    (tmp_path / "landed.jsonl").write_text(
        json.dumps({"ticket": "T-607", "branch": "task-t607", "merge_ts": t("06:40"), "gate_attempts": 1}) + "\n",
        encoding="utf-8")
    (tmp_path / "merge-queue.txt").write_text("", encoding="utf-8")
    (tmp_path / "merge-needs-attention.txt").write_text("", encoding="utf-8")
    return str(tmp_path)


NOW = datetime(2026, 9, 23, 8, 0)


def test_shape_with_no_flow_jsonl_and_no_open_experiment(monitor, tmp_path):
    ops = _ops(tmp_path)
    d = monitor.build_flow_panel(ops, now=NOW)
    assert "error" not in d
    for key in ("at", "ts", "landings", "hourly_24h", "gates_48h", "full_gate_p50_min",
                "causes_24h", "reds_24h", "gates_24h", "touchpoints_24h", "experiment"):
        assert key in d, key
    assert d["landings"]["series"] and d["landings"]["flow_jsonl"] == []
    assert d["experiment"] == {"open": False}
    assert d["reds_24h"] == 1 and d["gates_24h"] == 2
    assert d["causes_24h"].get("real") == 1
    # the killed/failed gate on task-t846 was `just test-ui` only -> class "ui", not "full",
    # so it doesn't feed full_gate_p50_min; the green bulk gate ran `just test` -> "full".
    assert d["full_gate_p50_min"] is not None


def test_landings_series_is_backfilled_and_rolling(monitor, tmp_path):
    ops = _ops(tmp_path)
    d = monitor.build_flow_panel(ops, now=NOW)
    series = d["landings"]["series"]
    assert all(set(p) >= {"hour", "ts", "landed", "roll6", "roll24"} for p in series)
    # one landing recorded at 06:40 -> the hour "09-23 06" row has landed=1, and every later
    # hour's rolling windows include it until it ages out of the 6h/24h window.
    by_hour = {p["hour"]: p for p in series}
    assert by_hour["09-23 06"]["landed"] == 1
    assert by_hour["09-23 07"]["roll6"] > 0
    assert by_hour["09-23 07"]["roll24"] > 0
    # covers roughly the last 48h, not the 72h of warm-up fetched for the rolling windows
    assert len(series) <= 50


def test_flow_jsonl_points_are_overlaid_and_garbage_lines_skipped(monitor, tmp_path):
    ops = _ops(tmp_path)
    (tmp_path / "flow.jsonl").write_text(
        "not json at all\n"
        + json.dumps({"ts": datetime(2026, 9, 23, 7, 30).timestamp(),
                       "landings_per_h_6h": 1.0, "landings_per_h_24h": 0.5}) + "\n"
        + json.dumps({"ts": datetime(2026, 9, 19, 1, 0).timestamp(),  # well outside the 48h window
                       "landings_per_h_6h": 9.0, "landings_per_h_24h": 9.0}) + "\n",
        encoding="utf-8")
    d = monitor.build_flow_panel(ops, now=NOW)
    assert "error" not in d
    pts = d["landings"]["flow_jsonl"]
    assert len(pts) == 1 and pts[0]["landings_per_h_24h"] == 0.5


def test_missing_flow_jsonl_is_not_an_error(monitor, tmp_path):
    ops = _ops(tmp_path)
    assert not os.path.exists(os.path.join(ops, "flow.jsonl"))
    d = monitor.build_flow_panel(ops, now=NOW)
    assert "error" not in d and d["landings"]["flow_jsonl"] == []


def test_open_experiment_block(monitor, tmp_path):
    ops = _ops(tmp_path)
    (tmp_path / "docs").mkdir()
    (tmp_path / "docs" / "ops-experiments.md").write_text("# ledger\n", encoding="utf-8")
    _, exp_mod = monitor._flow_modules()
    import argparse
    args = argparse.Namespace(
        id="E-9", hypothesis="test hypothesis", knob=["WORK_CAP=6"],
        baseline="2026-09-23 00:00..2026-09-23 05:00", metric="landings_per_h_24h",
        guard=["real_reds_24h <= baseline*1.25"], gates=6, hours=8.0,
        rule="keep if better", rollback="just knobs set WORK_CAP=4")
    opened = datetime(2026, 9, 23, 6, 0)
    assert exp_mod.cmd_new(ops, str(tmp_path), args, now=opened) == 0

    d = monitor.build_flow_panel(ops, now=NOW)
    assert "error" not in d
    e = d["experiment"]
    assert e["open"] is True
    assert e["id"] == "E-9" and e["hypothesis"] == "test hypothesis" and e["knobs"] == ["WORK_CAP=6"]
    assert e["rollback"] == "just knobs set WORK_CAP=4"
    assert e["metric"] == "landings_per_h_24h"
    assert isinstance(e["guards"], list) and e["guards"]
    for g in e["guards"]:
        assert set(g) == {"ok", "text"}


def test_garbage_experiments_jsonl_degrades_instead_of_raising(monitor, tmp_path):
    ops = _ops(tmp_path)
    (tmp_path / "experiments.jsonl").write_text("{not json\n{{{\n", encoding="utf-8")
    d = monitor.build_flow_panel(ops, now=NOW)
    assert "error" not in d
    assert d["experiment"] == {"open": False}


def test_hkpy_import_failure_degrades_to_an_error_body(monitor, tmp_path, monkeypatch):
    ops = _ops(tmp_path)

    def _boom():
        raise ImportError("simulated: hkpy unavailable")
    monkeypatch.setattr(monitor, "_flow_modules", _boom)
    d = monitor.build_flow_panel(ops, now=NOW)
    assert "error" in d and "at" in d


def test_flow_panel_cached_reuses_within_the_window(monitor, tmp_path):
    """One build per window. The build runs in a child process since 2026-09-24 (the dashboard's
    heap grew ~6 MB per in-process build), so the count is taken where every build goes through."""
    ops = _ops(tmp_path)
    monitor._FLOW_CACHE.update(at=0.0, data=None)
    calls = []
    real = monitor._child_json

    def counting(code, timeout=90):
        calls.append(1)
        return {"built": len(calls)}
    monitor._child_json = counting
    try:
        d1 = monitor.flow_panel_cached(ops, max_age=30.0)
        d2 = monitor.flow_panel_cached(ops, max_age=30.0)
        assert d1 is d2 and len(calls) == 1
    finally:
        monitor._child_json = real
        monitor._FLOW_CACHE.update(at=0.0, data=None)


def test_heavy_builds_run_in_a_child_and_errors_surface(monitor):
    assert monitor._child_json("import json, os; print(json.dumps({'pid': os.getpid()}))")["pid"] != __import__("os").getpid()
    try:
        monitor._child_json("raise SystemExit('the board is not strict YAML')")
    except RuntimeError as e:
        assert "the board is not strict YAML" in str(e)
    else:
        raise AssertionError("a failing child must raise")



def test_the_dashboard_re_executes_itself_above_its_rss_limit(monitor):
    seen = {}
    readings = iter([400.0, 900.0, 1300.0])
    out = monitor._rss_guard(limit_mb=1200, every_s=0, rss=lambda: next(readings),
                             execv=lambda exe, argv: seen.update(exe=exe, argv=argv) or "re-executed",
                             sleep=lambda s: None)
    assert out == "re-executed" and seen["argv"][0] == seen["exe"]


def test_flow_shows_which_tests_were_blamed_on_a_branch(monitor, tmp_path, monkeypatch):
    """Supervisor for the user, 2026-09-24 14:55: make the ledger's branch_defects visible on /flow."""
    (tmp_path / "flakes.json").write_text(json.dumps({"tests": {
        "canvas-journey.e2e.mjs": {"branch_defects": 3, "failed_alone": 0, "passed_alone": 2},
        "app-trace.e2e.mjs": {"branch_defects": 1, "failed_alone": 2, "passed_alone": 11},
        "quiet.e2e.mjs": {"branch_defects": 0}}}))
    monkeypatch.setattr(monitor, "SCRATCH", str(tmp_path))
    assert [(r["test"], r["blamed"]) for r in monitor.blamed_alone()] == [("canvas-journey.e2e.mjs", 3), ("app-trace.e2e.mjs", 1)]


def test_flow_panel_carries_the_queue_depth(monitor, tmp_path):
    (tmp_path / "merge-queue.txt").write_text("task-a\n")
    (tmp_path / "isolate-remaining").write_text("task-b task-c\n")
    d = monitor.build_flow_panel(str(tmp_path))
    assert d.get("queue_depth", {}).get("now", {}).get("waiting") == 3, d.get("error")
