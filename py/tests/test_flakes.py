"""The flake ledger — `py/hkpy/flakes.py`.

Written against the shapes the merge runner really emits (the lines below are copied from
`ops/merge-runner.log`, 2026-09-22), because the whole value of this file is that it counts
what the runner already forgives one gate at a time:

  * both directions are recorded — passed-alone is a load flake, failed-alone is a real defect,
    and the ledger must never collapse them into one "flaky" number;
  * the same incident reaching it from both sources (`flaky.jsonl` AND the TRIAGE lines) is
    ONE red, not two;
  * the threshold files a test once and then goes quiet, because a ledger that nags is a
    ledger that gets muted.
"""

from __future__ import annotations

import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from hkpy import flakes  # noqa: E402

YEAR = 2026


def log(*lines: str) -> str:
    """The runner writes every line TWICE (tee + an inherited redirect). So does this."""
    out = []
    for line in lines:
        out.append(line)
        out.append(line)
    return "\n".join(out) + "\n"


PASS_EPISODE = (
    "[09-22 15:33:48] TRIAGE: re-running the failing tests alone: tiles_batch_answers ",
    "[09-22 15:35:26] TRIAGE: they PASS alone -> load flake; retrying the full gate once",
)
FAIL_EPISODE = (
    "[09-22 15:17:38] TRIAGE: re-running the failing tests alone: the_view_lattices_floor ",
    "[09-22 15:18:50] TRIAGE: a test FAILS alone -> a real defect in this merge",
)
SPEC_EPISODE = (
    "[09-22 19:34:33] TRIAGE: browser specs red: canvas-journey.e2e.mjs - re-running them alone",
    "[09-22 19:37:35] TRIAGE: they PASS alone -> load flake; retrying the gate's acceptance phase once",
)


# ---------------------------------------------------------------------------
# Parsing
# ---------------------------------------------------------------------------


def test_both_directions_are_recorded_separately():
    incidents = flakes.parse_runner_log(log(*PASS_EPISODE, *FAIL_EPISODE, *SPEC_EPISODE), YEAR)
    assert len(incidents) == 3
    by_test = {i.tests[0]: i for i in incidents}
    assert by_test["tiles_batch_answers"].outcome == flakes.PASSED_ALONE
    assert by_test["the_view_lattices_floor"].outcome == flakes.FAILED_ALONE
    assert by_test["canvas-journey.e2e.mjs"].outcome == flakes.PASSED_ALONE

    ledger = flakes.build(incidents, now=time.mktime((YEAR, 9, 23, 0, 0, 0, 0, 0, -1)))
    assert ledger["tiles_batch_answers"].passed_alone == 1
    assert ledger["tiles_batch_answers"].failed_alone == 0
    assert ledger["the_view_lattices_floor"].failed_alone == 1
    assert ledger["the_view_lattices_floor"].passed_alone == 0
    # Every incident is a red-in-gate, whichever way it resolved. That is the count the
    # threshold reads: a test that costs a gate costs a gate.
    assert all(e.red_in_gate == 1 for e in ledger.values())


def test_a_multi_spec_red_counts_for_every_spec_in_it():
    text = log(
        "[09-22 18:30:14] TRIAGE: browser specs red: live-edge.e2e.mjs surface-address.e2e.mjs - re-running them alone",
        "[09-22 18:31:45] TRIAGE: a browser spec FAILS alone -> a real defect in this merge",
    )
    incidents = flakes.parse_runner_log(text, YEAR)
    assert incidents[0].tests == ("live-edge.e2e.mjs", "surface-address.e2e.mjs")
    ledger = flakes.build(incidents, now=time.mktime((YEAR, 9, 23, 0, 0, 0, 0, 0, -1)))
    assert set(ledger) == {"live-edge.e2e.mjs", "surface-address.e2e.mjs"}
    assert all(e.failed_alone == 1 for e in ledger.values())


def test_a_rerun_on_main_is_not_a_gate_red():
    """The runner also re-runs the same specs on a rewound main. Counting those doubles every
    browser incident, so the ledger must recognise and skip them."""
    text = log(
        "[09-22 17:56:52] TRIAGE: browser specs red: fog-of-war.e2e.mjs - re-running them alone",
        "[09-22 17:57:01] TRIAGE: a browser spec FAILS alone -> a real defect in this merge",
        "[09-22 17:57:01] TRIAGE: is main itself red? re-running the browser specs alone on the rewound main: fog-of-war.e2e.mjs",
        "[09-22 17:58:32] TRIAGE: MAIN IS RED on browser spec(s): fog-of-war.e2e.mjs -> batch re-queued in order, NOT isolated; queue the fix",
    )
    incidents = flakes.parse_runner_log(text, YEAR)
    assert len(incidents) == 1
    assert incidents[0].outcome == flakes.FAILED_ALONE


def test_a_suite_level_red_is_not_a_test():
    text = log("[09-22 13:11:30] TRIAGE: no FAIL lines found (lint/build failure?) - not a flake candidate")
    assert flakes.parse_runner_log(text, YEAR) == []


def test_flaky_jsonl_carries_the_load_the_log_does_not():
    text = (
        '{"ts":"2026-09-22T15:35:26","tests":"tiles_batch_answers ",'
        '"batch":"task-tilebody T-700","load_before":"30.63 31.34 29.72"}\n'
        "not json at all\n"
    )
    incidents = flakes.parse_flaky_jsonl(text)
    assert len(incidents) == 1
    assert incidents[0].outcome == flakes.PASSED_ALONE
    assert incidents[0].load == 30.63
    assert incidents[0].batch == "task-tilebody T-700"


def test_one_incident_from_two_sources_is_one_red():
    """The runner writes flaky.jsonl AND the TRIAGE line for the same pass-alone. One red."""
    log_incidents = flakes.parse_runner_log(log(*PASS_EPISODE), YEAR)
    jsonl = flakes.parse_flaky_jsonl(
        '{"ts":"2026-09-22T15:35:26","tests":"tiles_batch_answers ","batch":"b","load_before":"30.63 1 1"}\n'
    )
    merged = flakes.reconcile(log_incidents, jsonl)
    assert len(merged) == 1
    # ...and the log's incident has picked up the load only the jsonl knew.
    assert merged[0].load == 30.63
    assert merged[0].source == "log"

    ledger = flakes.build(merged, now=time.mktime((YEAR, 9, 23, 0, 0, 0, 0, 0, -1)))
    assert ledger["tiles_batch_answers"].red_in_gate == 1
    assert ledger["tiles_batch_answers"].loads == [30.63]


def test_a_jsonl_record_whose_log_line_rotated_away_is_still_counted():
    jsonl = flakes.parse_flaky_jsonl(
        '{"ts":"2026-09-20T01:00:00","tests":"old_test","batch":"b","load_before":"9 9 9"}\n'
    )
    merged = flakes.reconcile([], jsonl)
    assert [i.tests for i in merged] == [("old_test",)]


# ---------------------------------------------------------------------------
# The threshold
# ---------------------------------------------------------------------------


def two_reds(ops: str, test: str = "app-surface.e2e.mjs") -> None:
    with open(os.path.join(ops, flakes.RUNNER_LOG), "w") as fh:
        fh.write(
            log(
                f"[09-22 21:25:55] TRIAGE: browser specs red: {test} - re-running them alone",
                "[09-22 21:26:07] TRIAGE: they PASS alone -> load flake; retrying the gate's acceptance phase once",
                f"[09-22 21:55:43] TRIAGE: browser specs red: {test} - re-running them alone",
                "[09-22 21:55:54] TRIAGE: they PASS alone -> load flake; retrying the gate's acceptance phase once",
            )
        )
    with open(os.path.join(ops, flakes.FLAKY_JSONL), "w") as fh:
        fh.write(
            f'{{"ts":"2026-09-22T21:26:07","tests":"{test}","batch":"a","load_before":"12.09 10 9"}}\n'
            f'{{"ts":"2026-09-22T21:55:54","tests":"{test}","batch":"b","load_before":"10.57 10 9"}}\n'
        )
    os.utime(os.path.join(ops, flakes.RUNNER_LOG), (NOW, NOW))


NOW = time.mktime((YEAR, 9, 23, 2, 0, 0, 0, 0, -1))


def test_two_reds_in_seven_days_files_once_then_goes_quiet(tmp_path, monkeypatch):
    ops = str(tmp_path)
    two_reds(ops)
    posted: list[tuple] = []
    monkeypatch.setattr(flakes, "alert", lambda *a, **k: posted.append(a) or True)

    filed = flakes.update(ops, root=str(tmp_path), now=NOW)
    assert [e.test for e in filed] == ["app-surface.e2e.mjs"]
    line = filed[0].attention_line()
    assert line.startswith("FLAKY app-surface.e2e.mjs red 2x in 7d ")
    assert "(passed alone 2, failed alone 0; loads 12.09,10.57)" in line
    assert line.endswith("- needs a deterministic wait")

    needs = open(os.path.join(ops, flakes.NEEDS_FILE)).read()
    assert needs.count("FLAKY app-surface.e2e.mjs") == 1
    assert len(posted) == 1 and posted[0][1] == "amber"
    assert posted[0][4] == "flake:app-surface.e2e.mjs"

    # Re-running the tool (which the merge runner does after EVERY triage) must not re-file.
    for _ in range(3):
        assert flakes.update(ops, root=str(tmp_path), now=NOW) == []
    assert open(os.path.join(ops, flakes.NEEDS_FILE)).read().count("FLAKY") == 1
    assert len(posted) == 1


def test_one_red_is_an_incident_not_a_pattern(tmp_path, monkeypatch):
    ops = str(tmp_path)
    with open(os.path.join(ops, flakes.RUNNER_LOG), "w") as fh:
        fh.write(log(*PASS_EPISODE))
    os.utime(os.path.join(ops, flakes.RUNNER_LOG), (NOW, NOW))
    monkeypatch.setattr(flakes, "alert", lambda *a, **k: True)
    assert flakes.update(ops, root=str(tmp_path), now=NOW) == []
    assert not os.path.exists(os.path.join(ops, flakes.NEEDS_FILE))


def test_it_fires_again_every_two_further_reds(tmp_path, monkeypatch):
    ops = str(tmp_path)
    two_reds(ops)
    monkeypatch.setattr(flakes, "alert", lambda *a, **k: True)
    assert len(flakes.update(ops, root=str(tmp_path), now=NOW)) == 1

    # A third red: still quiet. A fourth: filed again, with the count that earned it.
    with open(os.path.join(ops, flakes.RUNNER_LOG), "a") as fh:
        fh.write(
            log(
                "[09-22 22:25:55] TRIAGE: browser specs red: app-surface.e2e.mjs - re-running them alone",
                "[09-22 22:26:07] TRIAGE: they PASS alone -> load flake; retrying the gate's acceptance phase once",
            )
        )
    assert flakes.update(ops, root=str(tmp_path), now=NOW) == []
    with open(os.path.join(ops, flakes.RUNNER_LOG), "a") as fh:
        fh.write(
            log(
                "[09-22 23:25:55] TRIAGE: browser specs red: app-surface.e2e.mjs - re-running them alone",
                "[09-22 23:26:07] TRIAGE: they PASS alone -> load flake; retrying the gate's acceptance phase once",
            )
        )
    filed = flakes.update(ops, root=str(tmp_path), now=NOW)
    assert [e.recent_red for e in filed] == [4]
    assert open(os.path.join(ops, flakes.NEEDS_FILE)).read().count("FLAKY") == 2


def test_an_old_red_falls_out_of_the_window(tmp_path, monkeypatch):
    ops = str(tmp_path)
    two_reds(ops)
    monkeypatch.setattr(flakes, "alert", lambda *a, **k: True)
    later = NOW + 30 * 86400
    entries = flakes.ledger(ops, now=later)
    e = entries["app-surface.e2e.mjs"]
    assert e.red_in_gate == 2 and e.recent_red == 0
    assert flakes.update(ops, root=str(tmp_path), now=later) == []


def test_a_test_that_fails_alone_is_filed_as_a_defect_not_a_wait(tmp_path, monkeypatch):
    ops = str(tmp_path)
    with open(os.path.join(ops, flakes.RUNNER_LOG), "w") as fh:
        fh.write(
            log(
                "[09-22 15:02:07] TRIAGE: re-running the failing tests alone: the_view_lattices_floor ",
                "[09-22 15:05:27] TRIAGE: a test FAILS alone -> a real defect in this merge",
                "[09-22 15:17:38] TRIAGE: re-running the failing tests alone: the_view_lattices_floor ",
                "[09-22 15:18:50] TRIAGE: a test FAILS alone -> a real defect in this merge",
            )
        )
    os.utime(os.path.join(ops, flakes.RUNNER_LOG), (NOW, NOW))
    monkeypatch.setattr(flakes, "alert", lambda *a, **k: True)
    filed = flakes.update(ops, root=str(tmp_path), now=NOW)
    assert len(filed) == 1
    assert "FAILS ALONE too" in filed[0].attention_line()
    assert filed[0].verdict().startswith("fails alone")


# ---------------------------------------------------------------------------
# Robustness — this is called from the merge runner
# ---------------------------------------------------------------------------


def test_an_empty_ops_directory_is_an_empty_ledger_not_a_crash(tmp_path):
    assert flakes.ledger(str(tmp_path)) == {}
    assert flakes.update(str(tmp_path), root=str(tmp_path)) == []
    assert flakes.render({})[0].startswith("flakes: nothing triaged yet")


def test_a_corrupt_state_file_is_ignored(tmp_path):
    path = os.path.join(str(tmp_path), flakes.LEDGER_JSON)
    open(path, "w").write("{not json")
    assert flakes.load_state(path) == {}
    open(path, "w").write(json.dumps({"tests": {"a": "not a dict"}}))
    assert flakes.load_state(path) == {}


def test_the_ledger_round_trips(tmp_path, monkeypatch):
    ops = str(tmp_path)
    two_reds(ops)
    monkeypatch.setattr(flakes, "alert", lambda *a, **k: True)
    flakes.update(ops, root=ops, now=NOW)
    data = json.load(open(os.path.join(ops, flakes.LEDGER_JSON)))
    rec = data["tests"]["app-surface.e2e.mjs"]
    assert rec["red_in_gate"] == 2 and rec["passed_alone"] == 2 and rec["notified_at"] == 2
    assert rec["loads"] == [12.09, 10.57]
    assert flakes.main(["--ops", ops, "--root", ops]) == 0



# ---------------------------------------------------------------------------
# Deflake requests (the user's rule, 2026-09-23)
# ---------------------------------------------------------------------------


def _spec_reds(n: int) -> str:
    eps = []
    for i in range(n):
        eps += [f"[09-2{2 if i < 3 else 3} 1{i}:34:33] TRIAGE: browser specs red: fog-of-war.e2e.mjs - re-running them alone",
                f"[09-2{2 if i < 3 else 3} 1{i}:37:35] TRIAGE: they PASS alone twice -> accepted as a load flake (the user's rule): just test-ui-e2e passes"]
    return log(*eps)


def test_the_third_isolation_pass_in_a_week_files_one_deflake_request(tmp_path, monkeypatch):
    ops = str(tmp_path)
    monkeypatch.setattr(flakes, "alert", lambda *a, **k: True)
    for n, want in ((2, 0), (3, 1), (3, 1), (4, 1), (5, 2)):
        with open(os.path.join(ops, flakes.RUNNER_LOG), "w") as fh:
            fh.write(_spec_reds(n))
        os.utime(os.path.join(ops, flakes.RUNNER_LOG), (NOW, NOW))
        flakes.update(ops, root=str(tmp_path), now=NOW + 86400)
        path = os.path.join(ops, flakes.DEFLAKE_REQUESTS)
        got = open(path).read().splitlines() if os.path.exists(path) else []
        assert len(got) == want, (n, got)
    req = json.loads(got[0])
    assert req["id"] == "deflake-fog-of-war-e2e-mjs" and req["kind"] == "spec" and req["count_7d"] == 3
    assert "fog-of-war.e2e.mjs: passed alone 3x in 7 days" in req["evidence"]
    assert all(i["outcome"] == flakes.PASSED_ALONE for i in req["incidents"])
    assert "DEFLAKE_REQUESTED fog-of-war.e2e.mjs" in open(os.path.join(ops, flakes.NEEDS_FILE)).read()


def test_a_failed_alone_red_never_counts_toward_a_deflake():
    e = flakes.Entry(test="t", first_seen=0, last_seen=0, recent_passed=2, failed_alone=5, recent_red=7)
    assert flakes.deflake_due({"t": e}) == []
    assert flakes.deflake_request(flakes.Entry(test="hk-cli::api_contract x", first_seen=0, last_seen=0), 0)["kind"] == "rust"



def test_the_deflake_counter_follows_the_window_down():
    e = flakes.Entry(test="t", first_seen=0, last_seen=0, recent_passed=3, deflaked_at=3)
    assert flakes.deflake_due({"t": e}) == []                    # just filed at 3
    e.recent_passed = 1                                           # week 1 aged out
    flakes.deflake_due({"t": e})
    e.recent_passed = 3                                           # three NEW flakes
    assert flakes.deflake_due({"t": e}) == [e]



def test_a_branch_breaking_a_spec_is_not_the_spec_being_flaky():
    """2026-09-23 23:01: T-801 broke three specs (failed alone on its merge) -> three FLAKY alarms."""
    single = log(
        "[09-23 22:57:49] TRIAGE: browser specs red: fog-of-war.e2e.mjs app-trace.e2e.mjs - re-running them alone",
        "[09-23 23:01:05] TRIAGE: a browser spec FAILS alone -> a real defect in this merge",
        "[09-23 23:01:07] GATE FAILED task-t801 (attempt 1/2, tip 7a5d243f) -> abort + flag for AI",
    )
    bulk = log(
        "[09-23 18:27:40] TRIAGE: browser specs red: fog-of-war.e2e.mjs - re-running them alone",
        "[09-23 18:28:18] TRIAGE: a browser spec FAILS alone -> a real defect in this merge",
        "[09-23 18:28:19] TRIAGE: is main itself red? re-running the browser specs alone on the rewound main: fog-of-war.e2e.mjs",
        "[09-23 18:30:00] TRIAGE: main is green on them -> the batch introduced it; isolating",
    )
    main_red = log(
        "[09-23 13:40:00] TRIAGE: browser specs red: surface-nav.e2e.mjs - re-running them alone",
        "[09-23 13:41:00] TRIAGE: a browser spec FAILS alone -> a real defect in this merge",
        "[09-23 13:41:01] TRIAGE: is main itself red? re-running the browser specs alone on the rewound main: surface-nav.e2e.mjs",
        "[09-23 13:47:00] TRIAGE: MAIN IS RED on browser spec(s): surface-nav.e2e.mjs -> batch re-queued in order",
    )
    incs = flakes.parse_runner_log(single + bulk + main_red, YEAR)
    assert [(i.tests, i.branch_defect) for i in incs] == [
        (("fog-of-war.e2e.mjs", "app-trace.e2e.mjs"), True), (("fog-of-war.e2e.mjs",), True),
        (("surface-nav.e2e.mjs",), False)]
    ledger = flakes.build(incs, now=NOW + 86400)
    assert ledger["fog-of-war.e2e.mjs"].recent_red == 0 and ledger["fog-of-war.e2e.mjs"].branch_defects == 2
    assert ledger["surface-nav.e2e.mjs"].recent_red == 1                 # main red on it: counts


def test_failed_alone_is_charged_only_to_the_specs_the_isolated_run_named():
    """09-22 16:25: four specs re-run alone, the tier named two as failed - app-trace passed that
    run yet was counted "failed alone", and its FLAKY verdict read "both ways" for two days."""
    text = log(
        "e2e: 10/14 files passed in 237.5 s (backend 2.3 s); failed: app-trace.e2e.mjs, fog-of-war.e2e.mjs",  # the gate's own run
        "[09-22 16:21:59] TRIAGE: browser specs red: app-trace.e2e.mjs fog-of-war.e2e.mjs surface-address.e2e.mjs - re-running them alone",
        "e2e: 1/3 files passed in 203.8 s (backend 1.9 s); failed: fog-of-war.e2e.mjs, surface-address.e2e.mjs",
        "[09-22 16:25:23] TRIAGE: a browser spec FAILS alone -> a real defect in this merge",
    )
    led = flakes.build(flakes.parse_runner_log(text, YEAR))
    assert (led["app-trace.e2e.mjs"].failed_alone, led["app-trace.e2e.mjs"].red_in_gate) == (0, 1)
    assert led["fog-of-war.e2e.mjs"].failed_alone == 1 and led["surface-address.e2e.mjs"].failed_alone == 1
    # no summary line (a Rust test set): every test in the set, as before
    led = flakes.build(flakes.parse_runner_log(log(*FAIL_EPISODE), YEAR))
    assert led["the_view_lattices_floor"].failed_alone == 1


def test_the_one_solo_pass_rule_reads_the_windowed_ledger():
    """User decision 2026-09-24 14:20: a test the ledger already shows passing alone (>= 2 in 7 d,
    never failing alone) is accepted after ONE solo pass; anything else keeps the twice rule."""
    now = time.time()
    day = 86400
    inc = flakes.Incident
    P, F = flakes.PASSED_ALONE, flakes.FAILED_ALONE
    incs = [inc(ts=now - 1 * day, tests=("app-trace.e2e.mjs",), outcome=P),
            inc(ts=now - 2 * day, tests=("app-trace.e2e.mjs",), outcome=P),
            inc(ts=now - 1 * day, tests=("fog-of-war.e2e.mjs",), outcome=P),
            inc(ts=now - 2 * day, tests=("fog-of-war.e2e.mjs",), outcome=P),
            inc(ts=now - 3 * day, tests=("fog-of-war.e2e.mjs",), outcome=F),
            inc(ts=now - 1 * day, tests=("once.e2e.mjs",), outcome=P),
            inc(ts=now - 9 * day, tests=("old.e2e.mjs",), outcome=P),
            inc(ts=now - 8 * day, tests=("old.e2e.mjs",), outcome=P),
            # 2026-09-24 11:19-11:30: passed alone before, then FAILED alone, pinned on the branch.
            inc(ts=now - 1 * day, tests=("canvas.e2e.mjs",), outcome=P),
            inc(ts=now - 2 * day, tests=("canvas.e2e.mjs",), outcome=P),
            inc(ts=now - 3 * day, tests=("canvas.e2e.mjs",), outcome=F, branch_defect=True),
            # a multi-spec red whose isolated run named ANOTHER spec as the failure
            inc(ts=now - 1 * day, tests=("a.e2e.mjs",), outcome=P),
            inc(ts=now - 2 * day, tests=("a.e2e.mjs",), outcome=P),
            inc(ts=now - 3 * day, tests=("a.e2e.mjs", "b.e2e.mjs"), outcome=F, failed_alone_tests=("b.e2e.mjs",))]
    since = now - 7 * day
    ok = lambda *t: flakes.solo_decision(incs, list(t), since)  # noqa: E731
    assert ok("app-trace.e2e.mjs") == (True, 2)
    assert ok("fog-of-war.e2e.mjs")[0] is False              # failed alone in the window
    assert ok("canvas.e2e.mjs")[0] is False                  # ... even when that fail was pinned on a branch
    assert ok("once.e2e.mjs")[0] is False                    # a first-time flaker: twice rule
    assert ok("old.e2e.mjs")[0] is False                     # its passes are outside the window
    assert ok("a.e2e.mjs") == (True, 2)                      # the isolated run named b, not a
    assert ok("app-trace.e2e.mjs", "once.e2e.mjs")[0] is False   # every test must qualify
    assert ok("never-seen.e2e.mjs")[0] is False and ok()[0] is False
    assert flakes.solo_decision(incs, ["fog-of-war.e2e.mjs"], now - 2.5 * day) == (True, 2)   # window start moves


def test_solo_query_counts_only_what_the_log_covers(tmp_path):
    """Fail-alones live only in the runner log, read from its last 40 MB: passes older than the log's
    first line must not count, and an unreadable log is no."""
    now = time.time()
    assert flakes.solo_query(str(tmp_path), ["x.e2e.mjs"], now=now) == (False, 0)
    from datetime import datetime as _dt
    stamp = lambda t: _dt.fromtimestamp(t).strftime("%m-%d %H:%M:%S")  # noqa: E731
    iso = lambda t: _dt.fromtimestamp(t).strftime("%Y-%m-%dT%H:%M:%S")  # noqa: E731
    (tmp_path / "merge-runner.log").write_text(f"[{stamp(now - 3600)}] merge-runner up\n")
    (tmp_path / "flaky.jsonl").write_text(
        "".join(f'{{"ts":"{iso(now - d * 86400)}","tests":"x.e2e.mjs","passes_alone":2,"accepted":true}}\n'
                for d in (2, 3, 4)))
    assert flakes.solo_query(str(tmp_path), ["x.e2e.mjs"], now=now)[0] is False   # all three predate the log


def test_a_spec_failing_alone_on_two_branches_in_a_day_is_main_side():
    """Supervisor for the user, 2026-09-24 14:55: canvas-journey failed alone on three merges, each
    counted a branch defect, so nothing ever said 'this is main's'. The lines are the runner's real
    order since main_is_red: its 'main is green on them -> not main's' comes BEFORE the verdict
    (review: fixtures without it hid that blamed_on was always empty)."""
    def single(t, branch, spec):
        return (f"[09-24 {t}:06] TRIAGE: browser specs red: {spec} - re-running them alone",
                f"[09-24 {t}:39] TRIAGE: a browser spec FAILS alone -> a real defect in this merge",
                f"[09-24 {t}:40] TRIAGE: is main itself red? re-running the browser specs alone on main: {spec}",
                f"[09-24 {t}:50] TRIAGE: main is green on them -> not main's",
                f"[09-24 {t}:51] GATE FAILED {branch} (attempt 1/2, tip 6bc3bd71) -> abort + flag for AI")
    text = log(*single("11:19", "task-t858", "app-trace.e2e.mjs"), *single("11:24", "task-t802", "app-trace.e2e.mjs"),
               "[09-24 13:48:40] TRIAGE: browser specs red: canvas-journey.e2e.mjs - re-running them alone",
               "[09-24 13:49:40] TRIAGE: a browser spec FAILS alone -> a real defect in this merge",
               "[09-24 13:49:41] TRIAGE: is main itself red? re-running the browser specs alone on main: canvas-journey.e2e.mjs",
               "[09-24 13:56:33] TRIAGE: main is green on them -> not main's",
               "[09-24 13:56:33] BULK gate FAILED -> rewound to 93e9ce27; isolate by merging each individually")
    incs = flakes.parse_runner_log(text, 2026)
    assert [i.blamed_on for i in incs] == ["task-t858", "task-t802", ""]      # a batch pins no single branch
    assert all(i.branch_defect for i in incs)
    now = incs[1].ts + 600
    assert flakes.main_side(incs, ["app-trace.e2e.mjs"], "task-t803", now) == {"app-trace.e2e.mjs": ["task-t802", "task-t858"]}
    assert flakes.main_side(incs, ["app-trace.e2e.mjs"], "task-t858", now) == {"app-trace.e2e.mjs": ["task-t802"]}
    assert flakes.main_side(incs, ["canvas-journey.e2e.mjs"], "task-t890", now) == {}   # only a batch before it
    assert flakes.main_side(incs, ["app-trace.e2e.mjs"], "task-t803", now + 2 * 86400) == {}   # older than 24 h


def test_a_main_side_verdict_counts_against_the_spec_not_the_branch():
    text = log(
        "[09-24 15:00:00] TRIAGE: browser specs red: canvas-journey.e2e.mjs - re-running them alone",
        "[09-24 15:01:00] TRIAGE: a browser spec FAILS alone -> a real defect in this merge",
        "[09-24 15:01:01] TRIAGE: is main itself red? re-running the browser specs alone on main: canvas-journey.e2e.mjs",
        "[09-24 15:04:00] TRIAGE: main is green on them -> not main's",
        "[09-24 15:05:00] TRIAGE: MAIN-SIDE canvas-journey.e2e.mjs: task-t890 -> task-t804 held, no attempt charged",
    )
    (inc,) = flakes.parse_runner_log(text, 2026)
    assert inc.outcome == flakes.FAILED_ALONE and not inc.branch_defect and inc.blamed_on == ""
    assert flakes.build([inc])["canvas-journey.e2e.mjs"].failed_alone == 1
