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
