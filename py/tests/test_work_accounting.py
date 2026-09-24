"""ops/work-runner.py's per-run resource accounting: what a ticket cost the box, and what it left.

`work-done.jsonl` recorded minutes, dollars and turns - what the MODEL spent. Nothing recorded
what the MACHINE did, so "which tickets are expensive to build" and "is the 3-core worker bound
holding" were both unanswerable from the record, and a leaked process tree was invisible until
someone ran `ps` (2026-09-22).

Two things here are easy to get wrong in ways no one would notice until they mattered:

  * **The tree is not the process group.** Measured on this box: a claim's process group contains
    only the `cpulimit` wrapper - `claude` and everything under it get their own groups. A
    `ps -g <pgid>` accounting would therefore report ~0 CPU-s for every ticket and look plausible.
  * **A leaked process cannot be found by ancestry**, because being reparented to launchd *is*
    the leak. So it is recognised by what this run was seen holding, or by its worktree path -
    and a pid must match one of those, because a SIGKILL aimed at a recycled pid is a far worse
    bug than the leak it was meant to clean up.
"""
from __future__ import annotations

import importlib.util
import pathlib
import sys

import pytest

_WR = pathlib.Path(__file__).resolve().parents[2] / "ops" / "work-runner.py"


def _load():
    spec = importlib.util.spec_from_file_location("hk_work_runner", _WR)
    mod = importlib.util.module_from_spec(spec)
    sys.modules["hk_work_runner"] = mod
    spec.loader.exec_module(mod)
    return mod


R = _load()
WT = "/Users/daniellewis/hackriff/.claude/worktrees/t513"


def rows(*specs):
    """(pid, ppid, pgid, rss_mb, cpu_s, cmd) tuples -> the row dicts the helpers consume."""
    return [{"pid": p, "ppid": pp, "pgid": g, "rss_mb": rss, "cpu_s": cpu, "cmd": cmd}
            for p, pp, g, rss, cpu, cmd in specs]


@pytest.mark.parametrize("text,secs", [
    ("0:00.01", 0.01), ("59:25.60", 3565.6), ("70:50.79", 4250.79),
    ("1:02:03.00", 3723.0), ("", 0.0), ("nonsense", 0.0),
])
def test_cpu_seconds_parses_the_ps_time_column(text, secs):
    assert R._cpu_seconds(text) == pytest.approx(secs)


def test_the_tree_is_not_the_process_group():
    """The wrapper is alone in its group; the real cost is in descendants with groups of their
    own. `ps -g <pgid>` would have reported 1.0 CPU-s for this run instead of 1301."""
    t = rows(
        (800, 1, 800, 5.0, 1.0, "cpulimit -l 300 -i -- claude -p"),
        (810, 800, 810, 300.0, 300.0, "claude -p --model sonnet"),
        (820, 810, 820, 50.0, 1000.0, "cargo nextest run -p hk-dsp"),
    )
    cpu, rss, pids, pgids = R.sample_group(800, t)
    assert cpu == 1301.0
    assert rss == 355.0
    assert pids == [800, 810, 820] and pgids == [800, 810, 820]


def test_members_of_the_roots_process_group_are_included_too():
    t = rows((800, 1, 800, 1.0, 1.0, "cpulimit"), (900, 1, 800, 2.0, 9.0, "a sibling in the group"))
    assert R.sample_group(800, t)[2] == [800, 900]


def test_a_tree_that_is_already_gone_samples_to_zero_not_an_error():
    assert R.sample_group(800, rows((1, 0, 1, 1.0, 1.0, "/sbin/launchd"))) == (0.0, 0.0, [], [])


def test_track_usage_keeps_the_high_water_mark(monkeypatch):
    """CPU time cannot be read at reap - the kernel discards it when the root exits - so the
    peak across ticks is all there is. A later, smaller sample must not lower it."""
    c = {"pid": 800}
    monkeypatch.setattr(R, "sample_group", lambda pid, rows=None: (500.0, 2048.0, [800], [800]))
    R.track_usage(c)
    monkeypatch.setattr(R, "sample_group", lambda pid, rows=None: (600.0, 100.0, [800, 810], [800]))
    R.track_usage(c)
    assert c["cpu_s"] == 600.0 and c["peak_rss_mb"] == 2048.0
    assert c["tree_pids"] == [800, 810]


def test_leak_is_found_by_worktree_path_when_ancestry_is_gone():
    """The leaked process has ppid 1 — that is what makes it a leak."""
    c = {"wt": WT, "tree_pids": [], "tree_pgids": []}
    t = rows((9001, 1, 9001, 400.0, 900.0, f"{WT}/target/debug/deps/two_device_e2e --exact x"))
    assert [r["pid"] for r in R.leaked_processes(c, t)] == [9001]


def test_leak_is_found_by_a_pid_and_group_this_run_was_seen_holding():
    c = {"wt": WT, "tree_pids": [9001], "tree_pgids": [9000]}
    t = rows((9001, 1, 9000, 10.0, 10.0, "some binary with no path"))
    assert [r["pid"] for r in R.leaked_processes(c, t)] == [9001]


def test_a_recycled_pid_alone_is_not_enough_to_be_killed():
    """Both guards, or neither. The pid matches but the group does not, and the command names no
    worktree: this is somebody else's process that inherited the number."""
    c = {"wt": WT, "tree_pids": [9001], "tree_pgids": [9000]}
    t = rows((9001, 1, 4242, 10.0, 10.0, "/Applications/Some.app/Contents/MacOS/Some"))
    assert R.leaked_processes(c, t) == []


def test_another_tickets_worktree_is_never_claimed_as_a_leak():
    c = {"wt": WT, "tree_pids": [], "tree_pgids": []}
    t = rows((9002, 1, 9002, 10.0, 10.0,
              "/Users/daniellewis/hackriff/.claude/worktrees/t607/target/debug/x"))
    assert R.leaked_processes(c, t) == []


def test_init_is_never_a_leak():
    c = {"wt": "", "tree_pids": [1], "tree_pgids": [1]}
    assert R.leaked_processes(c, rows((1, 0, 1, 1.0, 1.0, "/sbin/launchd"))) == []


def test_kill_leaked_escalates_term_then_kill(monkeypatch):
    sent = []
    alive = {9001, 9002}

    def fake_kill(pid, sig):
        import signal as S
        if sig == 0:
            if pid not in alive:
                raise ProcessLookupError(pid)
            return
        sent.append((pid, sig))
        if sig == S.SIGTERM and pid == 9001:
            alive.discard(9001)           # a well-behaved process exits on TERM

    monkeypatch.setattr(R.os, "kill", fake_kill)
    monkeypatch.setattr(R.time, "sleep", lambda s: None)
    killed = R.kill_leaked(rows((9001, 1, 9001, 1.0, 1.0, "a"), (9002, 1, 9002, 1.0, 1.0, "b")))
    import signal as S
    assert (9001, S.SIGTERM) in sent and (9002, S.SIGTERM) in sent
    assert killed == [9002] and (9001, S.SIGKILL) not in sent


def test_the_result_line_is_what_a_person_reads():
    assert R.resource_line({"cpu_s": 412.0, "peak_rss_mb": 1946.0}) == "Resources: 412 CPU-s, peak 1.9 GB"
    assert "LEAKED 3 processes" in R.resource_line({"cpu_s": 1.0, "peak_rss_mb": 1.0, "leaked": 3})
    assert R.resource_line({}) == ""      # a run with no sample says nothing, rather than "0"


def test_track_usage_reports_whether_the_claim_needs_writing(monkeypatch):
    """The runner only persists `work-claims.json` when something changed. A sampler that says
    nothing changed keeps its numbers in memory and loses them on restart - which is exactly the
    failure mode this accounting exists to end."""
    c = {"pid": 800}
    monkeypatch.setattr(R, "sample_group", lambda pid, rows=None: (500.0, 2048.0, [800], [800]))
    assert R.track_usage(c) is True
    assert R.track_usage(c) is False          # an identical sample is not a write
    monkeypatch.setattr(R, "sample_group", lambda pid, rows=None: (501.0, 2048.0, [800], [800]))
    assert R.track_usage(c) is True


# --------------------------------------------------------------------- conflicts (2026-09-23)
# A conflict-skipped branch waited for the coordinator: median 3.5 h from skip to landing over 16
# tickets, and 20 more never landed. It now goes back to its own worker like a gate failure.
NEEDS_LINES = (
    "09-23 15:26  task-t613  T-613  CONFLICT(skipped from bulk)\n"
    "09-23 16:11  task-t627  T-627  CONFLICT(skipped from bulk)\n"
    "09-23 16:20  task-t700  T-700  CONFLICT\n"
    "[09-23 13:47]  (bulk)  T-276 T-581  MAIN_RED - browser spec(s) fail on main itself\n"
    "09-23 09:10  task-gate-waiters  task-gate-waiters  GATE_FAIL\n"
)


def _claim(tid, branch, state="queued"):
    return {"ticket": tid, "branch": branch, "state": state, "session_id": "s-" + tid, "wt": "/tmp", "kind": "work",
            "started": 0}


@pytest.fixture
def conflicts(tmp_path, monkeypatch):
    needs = tmp_path / "merge-needs-attention.txt"
    needs.write_text(NEEDS_LINES)
    monkeypatch.setattr(R, "MERGE_NEEDS", str(needs))
    monkeypatch.setattr(R, "MERGE_QUEUE", str(tmp_path / "merge-queue.txt"))
    monkeypatch.setattr(R, "S", str(tmp_path))
    monkeypatch.setattr(R, "BULKMARK", str(tmp_path / "bulk-in-progress"))
    monkeypatch.setattr(R, "LOG", str(tmp_path / "work-runner.log"))
    monkeypatch.setattr(R, "NEEDS", str(tmp_path / "work-needs-attention.txt"))
    monkeypatch.setattr(R.os.path, "isdir", lambda p: True)
    monkeypatch.setattr(R, "gate_holds_dispatch", lambda: False)
    launched = []
    monkeypatch.setattr(R, "launch_fix", lambda c, line: launched.append((c["ticket"], line)) or dict(c, state="running", kind="fix"))
    monkeypatch.setattr(R, "merges_cleanly", lambda b, target="main": b == "task-t700")
    monkeypatch.setattr(R, "commits_ahead", lambda b, target: 3)
    monkeypatch.setattr(R, "board_statuses", lambda: {"T-613": "in-progress", "T-627": "in-progress", "T-700": "in-progress"})
    monkeypatch.setattr(R, "dispatch_cap", lambda: 4)
    return tmp_path, launched


def test_is_conflict_reads_both_runner_shapes_and_nothing_else():
    lines = NEEDS_LINES.splitlines()
    assert [R.is_conflict(ln) for ln in lines] == [True, True, True, False, False]


def test_a_conflicted_queued_branch_goes_back_to_its_worker_one_per_tick(conflicts):
    tmp, launched = conflicts
    claims = {"T-613": _claim("T-613", "task-t613"), "T-627": _claim("T-627", "task-t627"),
              "T-700": _claim("T-700", "task-t700")}
    assert R.handle_gate_failures(claims, dry=False)
    assert [t for t, _ in launched] == ["T-613"]                        # one conflict run per tick
    assert "CONFLICT(skipped from bulk)" in launched[0][1]
    assert "gate_fails_seen" not in claims["T-627"]                     # deferred, not dropped
    assert claims["T-700"]["gate_fails_seen"]                           # merges cleanly now: seen, re-queued
    assert (tmp / "merge-queue.txt").read_text().split() == ["task-t700"]
    R.handle_gate_failures(claims, dry=False)
    assert [t for t, _ in launched] == ["T-613", "T-627"]               # next tick takes the next one
    R.handle_gate_failures(claims, dry=False)
    assert len(launched) == 2                                           # never twice for one line


def test_a_landed_ticket_a_stale_line_or_an_empty_branch_gets_no_run(conflicts, monkeypatch):
    tmp, launched = conflicts
    monkeypatch.setattr(R, "board_statuses", lambda: {"T-613": "done", "T-627": "in-progress", "T-700": "todo"})
    claims = {"T-613": _claim("T-613", "task-t613"),                   # landed via a -rl branch
              "T-627": dict(_claim("T-627", "task-t627"), started=R._line_ts("09-23 16:11") + 3600),
              "T-700": _claim("T-700", "task-t700")}
    monkeypatch.setattr(R, "commits_ahead", lambda b, target: 0 if b == "task-t700" else 3)
    R.handle_gate_failures(claims, dry=False)
    assert launched == [] and all(c["gate_fails_seen"] for c in claims.values())
    assert not (tmp / "merge-queue.txt").exists()                      # nothing ahead: nothing re-queued


def test_an_unreadable_board_defers_every_conflict(conflicts, monkeypatch):
    tmp, launched = conflicts
    monkeypatch.setattr(R, "board_statuses", lambda: {})
    claims = {"T-613": _claim("T-613", "task-t613")}
    R.handle_gate_failures(claims, dry=False)
    assert launched == [] and "gate_fails_seen" not in claims["T-613"]


def test_a_conflict_run_waits_for_a_slot_and_for_holds(conflicts, monkeypatch):
    tmp, launched = conflicts
    busy = {f"W{i}": dict(_claim(f"W{i}", f"w{i}", "running"), kind=k) for i, k in enumerate(["work", "work", "fix", "work"])}
    claims = {"T-613": _claim("T-613", "task-t613"), **busy}
    R.handle_gate_failures(claims, dry=False)
    assert launched == []                                               # 4 busy (fix runs count) >= cap 4
    del claims["W0"]
    monkeypatch.setattr(R, "gate_holds_dispatch", lambda: True)          # alone mode: wait, never fix-held
    R.handle_gate_failures(claims, dry=False)
    assert launched == [] and "gate_fails_seen" not in claims["T-613"]
    monkeypatch.setattr(R, "gate_holds_dispatch", lambda: False)
    (tmp / "merge-queue.txt").write_text("task-t613\n")                # someone re-queued it by hand
    R.handle_gate_failures(claims, dry=False)
    assert launched == [] and claims["T-613"]["gate_fails_seen"]


def test_escalation_names_the_conflict_and_spends_no_slot(conflicts):
    tmp, launched = conflicts
    claims = {"T-613": dict(_claim("T-613", "task-t613"), fix_attempts=2), "T-627": _claim("T-627", "task-t627")}
    R.handle_gate_failures(claims, dry=False)
    assert "CONFLICT_ESCALATE" in (tmp / "work-needs-attention.txt").read_text()
    assert [t for t, _ in launched] == ["T-627"]


def test_fixes_merge_the_gated_base_while_a_batch_is_gating(tmp_path, monkeypatch):
    monkeypatch.setattr(R, "BULKMARK", str(tmp_path / "bulk-in-progress"))
    assert R.merge_target() == "main"
    (tmp_path / "bulk-in-progress").write_text("base=eab4bfae\nstarted=x\nbranches=a b\n")
    assert R.merge_target() == "eab4bfae"
    monkeypatch.setattr(R, "S", str(tmp_path))
    monkeypatch.setattr(R, "gate_holds_dispatch", lambda: False)
    seen = {}
    monkeypatch.setattr(R, "_run_fix", lambda c, n, prompt: seen.update(n=n, prompt=prompt) or c)
    R.launch_fix(_claim("T-613", "task-t613"), "09-23 15:26  task-t613  T-613  CONFLICT(skipped from bulk)")
    assert seen["n"] == 1 and "no longer merges cleanly" in seen["prompt"]
    assert "git merge eab4bfae" in seen["prompt"] and "git checkout eab4bfae -- docs/tasks.yaml" in seen["prompt"]
    R.launch_fix(_claim("T-1", "task-t1"), "09-23 09:10  task-t1  T-1  GATE_FAIL")
    assert "FAILED its merge gate on main" in seen["prompt"] and "git merge eab4bfae" in seen["prompt"]


def test_dispatch_counts_fix_runs_against_the_cap():
    claims = {"A": {"state": "running", "kind": "work"}, "B": {"state": "running", "kind": "fix"},
              "C": {"state": "running", "kind": "review"}, "D": {"state": "queued", "kind": "work"}}
    assert R.busy_workers(claims) == 2


# --------------------------------------------------------------------- dead dispatches (2026-09-23)
def test_a_dispatch_that_ended_with_nothing_goes_back_to_todo_after_the_release_window():
    now, old = 1_000_000.0, 1_000_000.0 - (R.RELEASE_AFTER_H + 1) * 3600
    tasks = {t: {"id": t, "status": s} for t, s in
             [("T-801", "in-progress"), ("T-512", "in-progress"), ("T-9", "in-progress"), ("T-10", "in-progress"),
              ("T-11", "in-progress"), ("T-12", "todo"), ("T-13", "in-progress")]}
    claims = {"T-801": {"state": "no-work", "started": old},          # killed 2 min in: revert
              "T-512": {"state": "timeout", "started": old},          # revert
              "T-9": {"state": "no-work", "started": now - 600},      # inside the window: wait
              "T-10": {"state": "no-work", "started": old},           # has commits: someone's work
              "T-11": {"state": "blocked", "started": old},           # a person's call, never automatic
              "T-12": {"state": "no-work", "started": old},           # already todo: release handles it
              "T-13": {"state": "error", "started": old}}             # an agent took it up since
    got = R.dead_dispatches(claims, tasks, now, has_work=lambda t: t == "T-10", busy={"T-13"})
    assert sorted(got) == ["T-512", "T-801"]


def test_has_work_fails_safe():
    assert R.has_work("T-does-not-exist-anywhere") is False
