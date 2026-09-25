"""The daily release candidate (user rule 2026-09-24): under GATE_TIERS=check the acceptance phase runs once a
day and on `just rc`, over main's landed tip, in ops/merge-runner.sh; green tags rc-YYYYMMDD, each red is one
P1 attention item. The REAL functions are extracted from the script and run in bash with the gate stubbed."""

import pathlib
import re
import subprocess

from hkpy import cycletime, flow

RUNNER = pathlib.Path(__file__).resolve().parents[2] / "ops" / "merge-runner.sh"


def _fn(name):
    m = re.search(rf"^{name}\(\)\{{.*?^\}}\n", RUNNER.read_text(), re.M | re.S)
    assert m, name
    return m.group(0)


RED_LOG = """gate: suites   = just acceptance-ci; just test-ui-e2e (phase: acceptance)
        FAIL [  91.965s] (27/82) hk-e2e::m0_slice a_replay_finds_the_fm_station
thread 'a_replay_finds_the_fm_station' panicked at tests/e2e/m0.rs:40: no detection near 100.8 MHz
gate: just acceptance-ci took 390s (exit 100)
gate: FAILED just acceptance-ci (exit 100)
"""
RESUMED_LOG = """✖ T-806: the layers menu has two axes
  AssertionError [ERR_ASSERTION]: overlays in paint order, defaults on
e2e: 5/6 files passed in 200 s (backend 1.4 s); failed: app-surface.e2e.mjs
gate: just test-ui-e2e took 230s (exit 1)
"""


def _run(tmp_path, first_rc, first_log, resumed_log=""):
    ops = tmp_path
    (ops / "log").write_text("")
    (ops / "first.log").write_text(first_log)
    (ops / "resumed.log").write_text(resumed_log)
    calls = ops / "calls"
    script = f"""
set -u
S={ops}; LOG={ops}/log; NEEDS={ops}/needs; REPO={ops}; RCMARK={ops}/rc-in-progress
log(){{ echo "[09-24 23:40:00] $*" >> {ops}/log; }}
notify_coordinator(){{ echo "NOTIFY $2" >> {calls}; }}
git(){{ case "$3" in rev-parse) echo 0123456789abcdef;; describe) echo rc-20260923;; tag) echo "TAG $5 $6" >> {calls};; esac; }}
n=0
limited(){{ n=$((n+1)); echo "GATE $*" >> {calls}; if [ $n = 1 ]; then cat {ops}/first.log >> {ops}/log; return {first_rc}; fi
            cat {ops}/resumed.log >> {ops}/log; return 1; }}
{_fn("run_rc")}
touch {ops}/rc-requested
run_rc
"""
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30)
    assert out.returncode == 0, out.stderr
    return (calls.read_text().splitlines() if calls.exists() else [],
            (ops / "needs").read_text() if (ops / "needs").exists() else "", ops)


def test_a_green_rc_tags_the_landed_tip_and_files_nothing(tmp_path):
    calls, needs, ops = _run(tmp_path, 0, "gate: just acceptance-ci took 390s (exit 0)\n")
    assert calls[0].startswith("GATE just gate --files crates/ --phase acceptance")
    assert any(c.startswith("TAG rc-") and c.endswith("0123456789abcdef") for c in calls)
    assert needs == "" and not (ops / "rc-requested").exists() and not (ops / "rc-in-progress").exists()
    assert "rc=0" in (ops / "rc-last").read_text()


def test_a_red_rc_still_runs_the_browser_tier_and_files_one_p1_item_per_red(tmp_path):
    calls, needs, ops = _run(tmp_path, 100, RED_LOG, RESUMED_LOG)
    assert calls[1] == "GATE just gate --files crates/ --phase acceptance --resume-after acceptance-ci"
    items = [line for line in needs.splitlines() if "RC_RED P1" in line]
    assert len(items) == 2
    rust = next(i for i in items if "a_replay_finds_the_fm_station" in i)
    assert "binary(m0_slice) & test(=a_replay_finds_the_fm_station)" in rust and "last green: rc-20260923" in rust
    assert "panicked at" in rust and "first red tip: 01234567" in rust
    spec = next(i for i in items if "app-surface.e2e.mjs" in i)
    assert "node e2e/run.mjs app-surface" in spec and "AssertionError" in spec
    assert not any(c.startswith("TAG") for c in calls) and "NOTIFY RC red - P1 tickets" in calls
    assert "RC RED" in (ops / "log").read_text()


def test_the_rc_is_due_once_a_day_after_its_hour_and_whenever_requested(tmp_path):
    def due(tiers, hour, last_today, requested):
        script = f"""
S={tmp_path}; GATE_TIERS={tiers}; RC_HOUR=3
date(){{ case "$1" in +%H) echo {hour:02d};; +%Y%m%d) echo 20260925;; esac; }}
{_fn("rc_due")}
rm -f $S/rc-requested $S/rc-last; {"touch $S/rc-requested;" if requested else ""} {"echo day=20260925 > $S/rc-last;" if last_today else ""}
rc_due && echo DUE || echo NO
"""
        return subprocess.run(["bash", "-c", script], capture_output=True, text=True).stdout.strip()
    assert due("check", 3, False, False) == "DUE"
    assert due("check", 2, False, False) == "NO"
    assert due("check", 9, True, False) == "NO"            # once a day
    assert due("full", 9, False, False) == "NO"            # every merge already runs the acceptance phase
    assert due("full", 1, True, True) == "DUE"             # `just rc` always


def test_the_rc_suites_are_credited_to_no_merge_gate():
    log = """[09-24 23:00:00] BULK attempt (2): task-a task-b
[09-24 23:00:01] BULK gate (just gate --base abc --phase check over 2 merged branches; may take 15-25 min)…
gate: just lint took 100s (exit 0)
[09-24 23:20:00] BULK MERGED ✓ task-a task-b
[09-24 23:21:00] RC gate (just gate --files crates/ --phase acceptance on main 01234567)…
gate: just acceptance-ci took 390s (exit 0)
gate: just test-ui-e2e took 230s (exit 0)
[09-24 23:32:00] RC GREEN 01234567 -> tagged rc-20260924
"""
    gates = flow.gates_from(flow.parse_log(log, 2026))
    runs = cycletime.parse_suite_runs(log, 2026)
    assert [[c for c, _, _ in r.suites] for r in runs] == [["just lint"]]
    assert [[c for c, _, _ in g.suites] for g in gates] == [["just lint"]]
