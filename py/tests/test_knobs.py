"""The knob store and the bounded hold enforce the pipeline-manager invariants 3-6 in code."""

import json

from hkpy import knobs


def test_set_writes_the_store_and_the_log_and_names_the_reader(tmp_path, capsys):
    ops = str(tmp_path)
    assert knobs.cmd_set(ops, ["WORK_CAP=6", "WORK_QUEUE_PAUSE=12"], why="trial", who="t") == 0
    assert knobs.read_store(ops) == {"WORK_CAP": "6", "WORK_QUEUE_PAUSE": "12"}
    rec = json.loads((tmp_path / "env.jsonl").read_text().splitlines()[-1])
    assert rec["set"] == {"WORK_CAP": "6", "WORK_QUEUE_PAUSE": "12"} and rec["kind"] == "incident"
    out = capsys.readouterr().out
    assert "INCIDENT CHANGE" in out and "work-runner" in out


def test_set_under_an_open_experiment_is_an_experiment_change(tmp_path):
    ops = str(tmp_path)
    (tmp_path / "experiments.jsonl").write_text(json.dumps({"event": "open", "id": "E-9"}) + "\n")
    knobs.cmd_set(ops, ["BULK_MAX=10"], why="", who="t")
    rec = json.loads((tmp_path / "env.jsonl").read_text().splitlines()[-1])
    assert rec["experiment"] == "E-9" and rec["kind"] == "experiment"


def test_set_refuses_cap_zero_and_unknown_keys(tmp_path):
    ops = str(tmp_path)
    assert knobs.cmd_set(ops, ["WORK_CAP=0"], why="", who="t") == 3
    assert knobs.cmd_set(ops, ["NOT_A_KNOB=1"], why="", who="t") == 2
    assert knobs.read_store(ops) == {}


def test_set_refuses_values_the_readers_cannot_parse(tmp_path):
    """A typo in the store would kill the work runner at its next start (review, 2026-09-23)."""
    ops = str(tmp_path)
    assert knobs.cmd_set(ops, ["BULK_MAX=abc"], why="", who="t") == 2
    assert knobs.cmd_set(ops, ["WORK_CAP=-1"], why="", who="t") == 2
    assert knobs.cmd_set(ops, ["BULK_MAX=0"], why="", who="t") == 2
    assert knobs.cmd_set(ops, ["WORKER_DRAIN_MAX=0", "WORK_CAP=1"], why="", who="t") == 0
    assert knobs.read_store(ops) == {"WORKER_DRAIN_MAX": "0", "WORK_CAP": "1"}


def test_an_expired_marker_left_by_a_dead_runner_does_not_block_the_next_hold(tmp_path):
    ops = str(tmp_path)
    (tmp_path / "hold").write_text("until=100\nsince=40\nowner=p\nwhy=old\n")
    assert knobs.cmd_hold(ops, 10, "new", "p", now=10_000.0, repo=str(tmp_path), do_alert=False) == 0
    assert knobs.read_hold(ops)["why"] == "new"


def test_why_is_flattened_to_one_line(tmp_path):
    ops = str(tmp_path)
    assert knobs.cmd_hold(ops, 5, "line one\n  line two\ttabbed", "p", now=0.0, repo=str(tmp_path), do_alert=False) == 0
    assert knobs.read_hold(ops)["why"] == "line one line two tabbed"


def test_unset_and_reset(tmp_path):
    ops = str(tmp_path)
    knobs.cmd_set(ops, ["WORK_CAP=6", "BULK_MAX=10"], why="", who="t")
    knobs.cmd_unset(ops, ["WORK_CAP"], who="t")
    assert knobs.read_store(ops) == {"BULK_MAX": "10"}
    knobs.cmd_reset(ops, who="t")
    assert knobs.read_store(ops) == {}


def test_effective_reads_the_running_scripts_start_lines(tmp_path):
    (tmp_path / "work-runner.log").write_text(
        "[09-23 12:33:00] VERSION: matches HEAD  ops=/x cap=6 dry=False\n"
        "[09-23 12:33:00] KNOBS: WORK_CAP=6 WORK_QUEUE_PAUSE=12 WORK_GATE_ALONE=0\n")
    (tmp_path / "merge-runner.log").write_text("[09-23 12:14:41] KNOBS: WORKER_DRAIN_MAX=0 BULK_MAX=15\n")
    eff = knobs.effective(str(tmp_path))
    assert eff["WORK_CAP"] == "6" and eff["WORK_GATE_ALONE"] == "0" and eff["WORKER_DRAIN_MAX"] == "0"


def test_hold_writes_the_marker_within_the_limit(tmp_path):
    ops = str(tmp_path)
    assert knobs.cmd_hold(ops, 20, "incident: main red", "pipeline", now=1000.0, repo=str(tmp_path), do_alert=False) == 0
    h = knobs.read_hold(ops)
    assert h["until"] == "2200" and h["why"] == "incident: main red" and h["owner"] == "pipeline"
    rec = json.loads((tmp_path / "hold.jsonl").read_text().splitlines()[-1])
    assert rec["event"] == "hold" and rec["minutes"] == 20


def test_hold_refuses_more_than_thirty_minutes_and_needs_a_why(tmp_path):
    ops = str(tmp_path)
    assert knobs.cmd_hold(ops, 31, "x", "p", now=0.0, repo=str(tmp_path), do_alert=False) == 3
    assert knobs.cmd_hold(ops, 10, "  ", "p", now=0.0, repo=str(tmp_path), do_alert=False) == 2
    assert knobs.read_hold(ops) is None


def test_hold_refuses_a_second_within_two_hours(tmp_path):
    ops = str(tmp_path)
    assert knobs.cmd_hold(ops, 10, "a", "p", now=0.0, repo=str(tmp_path), do_alert=False) == 0
    assert knobs.cmd_release(ops, "p", now=600.0) == 0
    assert knobs.cmd_hold(ops, 10, "b", "p", now=3600.0, repo=str(tmp_path), do_alert=False) == 3
    assert knobs.cmd_hold(ops, 10, "b", "p", now=2 * 3600.0 + 1, repo=str(tmp_path), do_alert=False) == 0


def test_hold_refuses_while_a_branch_is_queued_or_a_gate_runs(tmp_path):
    ops = str(tmp_path)
    (tmp_path / "merge-queue.txt").write_text("task-x\n")
    assert knobs.cmd_hold(ops, 10, "a", "p", now=0.0, repo=str(tmp_path), do_alert=False) == 3
    (tmp_path / "merge-queue.txt").write_text("# only a comment\n")
    (tmp_path / "bulk-in-progress").write_text("")
    assert knobs.cmd_hold(ops, 10, "a", "p", now=0.0, repo=str(tmp_path), do_alert=False) == 3
    (tmp_path / "bulk-in-progress").unlink()
    assert knobs.cmd_hold(ops, 10, "a", "p", now=0.0, repo=str(tmp_path), do_alert=False) == 0


def test_release_records_the_minutes_held(tmp_path):
    ops = str(tmp_path)
    knobs.cmd_hold(ops, 10, "a", "p", now=0.0, repo=str(tmp_path), do_alert=False)
    knobs.cmd_release(ops, "p", now=300.0)
    rec = json.loads((tmp_path / "hold.jsonl").read_text().splitlines()[-1])
    assert rec["event"] == "release" and rec["held_minutes"] == 5.0 and knobs.read_hold(ops) is None


def test_gate_tiers_takes_a_word_and_only_full_or_check(tmp_path):
    """User, 2026-09-24: GATE_TIERS=check gates merges with the check phase only."""
    ops = str(tmp_path)
    assert knobs.cmd_set(ops, ["GATE_TIERS=all"], why="", who="t") == 2
    assert knobs.cmd_set(ops, ["GATE_TIERS=1"], why="", who="t") == 2
    assert knobs.read_store(ops) == {}
    assert knobs.cmd_set(ops, ["GATE_TIERS=check"], why="", who="t") == 0
    assert knobs.read_store(ops) == {"GATE_TIERS": "check"}


def test_the_merge_runner_passes_the_gate_tier_to_every_merge_gate():
    """Every gate call the runner makes on a merge carries $GATE_PHASE, and the retry commands too."""
    import pathlib, re
    src = (pathlib.Path(__file__).resolve().parents[2] / "ops" / "merge-runner.sh").read_text()
    calls = re.findall(r"limited just gate(?:-merge)?[^;\n]*", src)
    assert calls and all("$GATE_PHASE" in c for c in calls), calls
    assert '"just gate-merge $GATE_PHASE"' in src and 'retry=${4:-"just gate --base $base $GATE_PHASE"}' in src
