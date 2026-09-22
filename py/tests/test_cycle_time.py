"""T-763: WHICH SUITE MOVED, and what counts as a measurement of it.

The board said the merge gate had gone from a 21.4 min median to ~34 in two days. Answering
that needed per-suite history, and `$HACKRIFF_OPS/gate-timings.jsonl` did not have it: on
2026-09-22 it held 33 runs, of which 31 were written by `py/tests/test_gate.py` itself
(their `root` is a pytest tmpdir — fixed there) and exactly TWO were real `full` gates,
against 48 full gates the merge runner had actually run over the same window.

The record that survived is the line `py/hkpy/gate.py` PRINTS beside each
`gatelog.append()` — `gate: just test took 1221s (exit 0)` — which `merge-runner.log` has
captured since before the structured log existed, and which is per suite. `parse_suite_runs`
reads it back.

These tests pin the three rules that make that history mean something:

  * duplicated lines collapse (the runner's `log()` both tees and inherits a redirect, so
    every line lands twice);
  * an ABORTED run is not a measurement of the suite — the gate stops at the first failure,
    so a failed run measures a prefix;
  * two CLASSES are never pooled into one median. Pooling is how a 16-second `py` gate and a
    22-minute `full` one came to share a "gate median" of 21.4 min in `CLAUDE.md`, which is
    most of the apparent 60 % regression this ticket was filed for.
"""

from __future__ import annotations

from hkpy import gatelog
from hkpy.cycletime import parse_suite_runs, rolling_medians, suite_stats

_RUNNER_LOG = """\
[09-20 23:20:00] GATE task-a (just gate-merge; may take 15-25 min)...
gate: just lint took 60s (exit 0)
gate: just lint took 60s (exit 0)
gate: just test took 900s (exit 0)
gate: just acceptance-ci took 200s (exit 0)
[09-20 23:40:00] MERGED task-a
[09-21 00:00:00] GATE task-b (just gate-merge; may take 15-25 min)...
gate: just lint took 60s (exit 0)
gate: just test took 400s (exit 100)
[09-21 00:10:00] GATE FAILED task-b (attempt 1/2, tip deadbeef) -> abort + flag for AI
[09-21 01:00:00] BULK gate (just gate --base abc over 3 merged branches; may take 15-25 min)...
gate: just lint took 70s (exit 0)
gate: just test took 1100s (exit 0)
gate: just acceptance-ci took 300s (exit 0)
[09-21 02:00:00] GATE task-c (just gate-merge; may take 15-25 min)...
gate: just lint-py took 0s (exit 0)
gate: just test-py took 16s (exit 0)
[09-21 02:01:00] MERGED task-c
"""


def test_per_suite_durations_are_recovered_from_the_runner_log():
    runs = parse_suite_runs(_RUNNER_LOG, 2026)

    assert [len(r.suites) for r in runs] == [3, 2, 3, 2]
    # The duplicate `lint` line must collapse, or every suite would read double.
    assert runs[0].suites == [
        ("just lint", 60, 0),
        ("just test", 900, 0),
        ("just acceptance-ci", 200, 0),
    ]
    assert runs[0].seconds == 1160
    assert runs[0].cost("just test") == 900
    assert runs[0].cost("just test-ui-e2e") is None
    # A BULK gate is a gate; it opens a run exactly as a branch gate does.
    assert runs[2].cost("just test") == 1100


def test_an_aborted_run_is_not_a_measurement_of_the_suite():
    runs = parse_suite_runs(_RUNNER_LOG, 2026)

    assert [r.passed for r in runs] == [True, False, True, True]
    # task-b failed inside the Rust suite and never reached the acceptance one at all.
    # Counted, it would say the gate got FASTER whenever an unrelated test broke.
    assert runs[1].seconds == 460


def test_suite_stats_never_pools_two_classes_into_one_median():
    text = _RUNNER_LOG
    for hour in range(3, 9):  # enough complete runs of one class to compare halves
        text += (
            f"[09-21 0{hour}:00:00] GATE task-{hour} (just gate-merge)...\n"
            "gate: just lint took 60s (exit 0)\n"
            "gate: just test took 1000s (exit 0)\n"
            "gate: just acceptance-ci took 250s (exit 0)\n"
        )
    blob = "\n".join(suite_stats(parse_suite_runs(text, 2026)))

    assert "CLASS [lint + test + acceptance-ci]" in blob
    assert "CLASS [lint-py + test-py]" in blob
    # The aborted run contributes to neither — it is not a complete passing run.
    assert "CLASS [lint + test]" not in blob


def test_suite_stats_says_so_rather_than_guessing_from_two_runs():
    blob = "\n".join(suite_stats(parse_suite_runs(_RUNNER_LOG, 2026)))

    # Three complete passing runs, no two of the same class. Nothing here may be reported as
    # a trend; a tool that invents one from n=1 is how impressions get laundered into data.
    assert "too few to compare halves." in blob


def _finished(klass: str, seconds: float, n: int, rc: int = 0) -> list[dict]:
    out: list[dict] = []
    for _ in range(n):
        rid = gatelog.new_run_id()
        out.append(
            gatelog.start_record(rid, klass=klass, phase="all", source="s", n_files=1)
        )
        out.append(
            gatelog.end_record(rid, klass=klass, phase="all", seconds=seconds, rc=rc)
        )
    return out


def test_a_failed_run_is_left_out_of_the_budget_median():
    records = _finished("full", 30 * 60, 5) + _finished("full", 120.0, 5, rc=101)
    median, n = rolling_medians(gatelog.runs(records))["full"]

    # Five two-minute aborts would drag a pooled median to two minutes and make a broken
    # suite look like a fast one. Only the runs that ran the whole thing are measurements.
    assert n == 5 and median == 30 * 60
