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
import json
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
    monkeypatch.setattr(R, "REPO", str(tmp_path))                     # never the live .git/MERGE_HEAD
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
              ("T-11", "in-progress"), ("T-12", "todo"), ("T-13", "in-progress"), ("T-14", "in-progress"),
              ("T-15", "in-progress")]}
    tasks["T-14"]["branch"] = "fix/my-own-take"                          # a person took it by hand
    claims = {"T-801": {"state": "no-work", "started": old},          # killed 2 min in: revert
              "T-512": {"state": "timeout", "started": old},          # revert
              "T-9": {"state": "no-work", "started": now - 600},      # inside the window: wait
              "T-10": {"state": "no-work", "started": old},           # has commits: someone's work
              "T-11": {"state": "blocked", "started": old},           # a person's call, never automatic
              "T-12": {"state": "no-work", "started": old},           # already todo: release handles it
              "T-13": {"state": "error", "started": old},             # an agent took it up since
              "T-14": {"state": "no-work", "started": old},
              "T-15": {"state": "no-work", "started": old}}           # its SECOND dead run: a person
    revert, skipped = R.dead_dispatches(claims, tasks, now, has_work=lambda t: t == "T-10",
                                        busy={"T-13": old + 60, "T-801": old - 60}, prior={"T-801": 1, "T-15": 2})
    assert sorted(revert) == ["T-512", "T-801"] and skipped == ["T-15"]


def test_dead_outcomes_counts_only_dead_work_runs(tmp_path, monkeypatch):
    done = tmp_path / "work-done.jsonl"
    rows = [("T-1", "work", "no-work"), ("T-1", "work", "timeout"), ("T-1", "review", "no-work"),
            ("T-2", "work", "done"), ("T-3", "work", "error")]
    done.write_text("".join(json.dumps({"ticket": t, "kind": k, "outcome": o}) + "\n" for t, k, o in rows) + "garbage\n")
    monkeypatch.setattr(R, "DONE", str(done))
    assert R.dead_outcomes() == {"T-1": 2, "T-3": 1}


def test_no_branch_means_no_work_and_an_error_means_work(monkeypatch):
    assert R.has_work("T-does-not-exist-anywhere") is False
    monkeypatch.setattr(R, "sh", lambda *a, **k: (_ for _ in ()).throw(RuntimeError("git gone")))
    assert R.has_work("T-801") is True                                  # cannot tell: never revert


def test_a_branch_the_merge_runner_holds_is_being_merged_not_conflicted(conflicts):
    tmp, launched = conflicts
    (tmp / "bulk-in-progress").write_text("base=abc\nbranches=task-t627 task-x\n")
    assert R.merging_branches() == {"task-t627", "task-x"}
    claims = {"T-627": _claim("T-627", "task-t627")}
    R.handle_gate_failures(claims, dry=False)
    assert launched == [] and claims["T-627"]["gate_fails_seen"]
    assert not (tmp / "merge-queue.txt").exists()


# --------------------------------------------------------------------- deflake dispatch (2026-09-23)
# The user's decision: the 3rd isolation-pass of one test within 7 days auto-spawns a deflaker.
# py/hkpy/flakes.py appends the request; the work runner dispatches it as a worker whose claim is
# keyed DEFLAKE:<slug> - not a board ticket, so no ticket-shaped path may act on it.

DAY = 86400.0


def _req(rid, ts, test="fog-of-war.e2e.mjs", kind="spec", **kw):
    return dict({"ts": ts, "id": rid, "test": test, "kind": kind, "count_7d": 3,
                 "incidents": [{"ts": "09-23 10:00", "batch": "task-t1 task-t2", "load_before": "31.2",
                                "outcome": "passed-alone"}],
                 "evidence": "merge-runner.log 09-23 10:00: red, passed alone twice"}, **kw)


@pytest.fixture
def df(tmp_path, monkeypatch):
    """Everything the deflake path touches, pointed at tmp; nothing is ever spawned."""
    reqs = tmp_path / "deflake-requests.jsonl"
    for name, val in [("S", str(tmp_path)), ("DEFLAKE_REQUESTS", str(reqs)), ("WORKDIR", str(tmp_path / "work")),
                      ("LOG", str(tmp_path / "work-runner.log")), ("NEEDS", str(tmp_path / "work-needs-attention.txt")),
                      ("DONE", str(tmp_path / "work-done.jsonl")), ("MERGE_QUEUE", str(tmp_path / "merge-queue.txt")),
                      ("MERGE_NEEDS", str(tmp_path / "merge-needs-attention.txt")),
                      ("BULKMARK", str(tmp_path / "bulk-in-progress"))]:
        monkeypatch.setattr(R, name, val)
    monkeypatch.setattr(R, "gate_holds_dispatch", lambda: False)
    monkeypatch.setattr(R, "dispatch_cap", lambda: 4)
    monkeypatch.setattr(R, "disk_free_gb", lambda: 500.0)
    seen = []
    monkeypatch.setattr(R, "attention", lambda t, b, k, d="": seen.append((t, k, d)))
    launched = []

    def fake_launch(slug, req, prior, dry):
        launched.append((slug, req["ts"], (prior or {}).get("run", 0) + 1))
        return {"ticket": "DEFLAKE:" + slug, "deflake": slug, "kind": "deflake", "state": "running", "pid": 1,
                "started": 1e9, "branch": "task-" + slug, "wt": "/nonexistent", "request_ts": req["ts"],
                "run": (prior or {}).get("run", 0) + 1}
    monkeypatch.setattr(R, "launch_deflake", fake_launch)

    def write(*rows):
        reqs.write_text("".join((r if isinstance(r, str) else json.dumps(r)) + "\n" for r in rows))
    return tmp_path, write, launched, seen


def test_requests_parse_and_garbage_lines_are_skipped(df):
    tmp, write, _, _ = df
    write("not json", "[1, 2]", "", {"id": "x", "test": "t"},                       # no ts
          {"ts": True, "id": "x", "test": "t"}, {"ts": 1.0, "id": "", "test": "t"},  # a bool ts, an empty id
          {"ts": 1.0, "id": "!!!", "test": "t"}, {"ts": 1.0, "id": "x", "test": " "},
          _req("deflake-fog-of-war-e2e-mjs", 5.0),
          _req("deflake-hk-cli::api_contract the/band collapsed", 6.0, test="hk-cli::api_contract x", kind="rust"))
    got = R.read_deflake_requests()
    assert [r["slug"] for r in got] == ["deflake-fog-of-war-e2e-mjs", "deflake-hk-cli-api-contract-the-band-collapsed"]
    assert got[1]["kind"] == "rust" and got[1]["incidents"][0]["outcome"] == "passed-alone"
    assert R.read_deflake_requests(str(tmp / "absent.jsonl")) == []


def test_one_claim_per_id_and_none_while_it_is_open(df):
    tmp, write, launched, _ = df
    write(_req("deflake-a", 100.0), _req("deflake-a", 200.0))       # the same id twice (3rd and 4th flake)
    claims = {}
    assert R.dispatch_deflakes(claims, dry=False)
    assert launched == [("deflake-a", 200.0, 1)]                      # one run, from the newest request
    assert list(claims) == ["DEFLAKE:deflake-a"] and claims["DEFLAKE:deflake-a"]["state"] == "running"
    write(_req("deflake-a", 100.0), _req("deflake-a", 200.0), _req("deflake-a", 300.0))
    R.dispatch_deflakes(claims, dry=False)
    R.dispatch_deflakes(claims, dry=False)
    assert len(launched) == 1                                         # open: the 5th flake waits
    log = (tmp / "work-runner.log").read_text()
    assert log.count("DEFLAKE WAIT deflake-a") == 1 and "still running" in log   # logged once, not per tick


def test_a_new_request_waits_24h_after_the_last_run_ended(df, monkeypatch):
    tmp, write, launched, _ = df
    now = 10 * DAY
    monkeypatch.setattr(R.time, "time", lambda: now)
    claims = {"DEFLAKE:deflake-a": {"ticket": "DEFLAKE:deflake-a", "deflake": "deflake-a", "kind": "review",
                                    "state": "queued", "run": 1, "request_ts": now - 2 * DAY, "ended": now - 2 * 3600}}
    write(_req("deflake-a", now - 2 * DAY), _req("deflake-a", now - 60))
    R.dispatch_deflakes(claims, dry=False)
    assert launched == [] and "waits until 24 h" in (tmp / "work-runner.log").read_text()
    write(_req("deflake-a", now - 2 * DAY))                            # only the consumed request: nothing to do
    claims["DEFLAKE:deflake-a"]["ended"] = now - 25 * 3600
    R.dispatch_deflakes(claims, dry=False)
    assert launched == []
    write(_req("deflake-a", now - 2 * DAY), _req("deflake-a", now - 60))
    R.dispatch_deflakes(claims, dry=False)
    assert launched == [("deflake-a", now - 60, 2)]                   # run 2, past the wait


def test_the_cap_and_one_per_tick(df, monkeypatch):
    tmp, write, launched, _ = df
    write(_req("deflake-a", 1.0), _req("deflake-b", 2.0), _req("deflake-c", 3.0))
    busy = {f"T-{i}": {"state": "running", "kind": k} for i, k in enumerate(["work", "fix", "work", "deflake"])}
    claims = dict(busy)
    R.dispatch_deflakes(claims, dry=False)
    assert launched == []                                             # 4 busy (a deflake counts) >= cap 4
    del claims["T-0"]
    R.dispatch_deflakes(claims, dry=False)
    assert [s for s, _, _ in launched] == ["deflake-a"]               # oldest first, one per tick
    R.dispatch_deflakes(claims, dry=False)
    assert len(launched) == 1                                         # the new deflake took the free slot
    monkeypatch.setattr(R, "dispatch_cap", lambda: 8)
    R.dispatch_deflakes(claims, dry=False)
    assert [s for s, _, _ in launched] == ["deflake-a", "deflake-b"]


def test_holds_are_respected(df, monkeypatch):
    tmp, write, launched, _ = df
    write(_req("deflake-a", 1.0))
    (tmp / "dispatch-paused").write_text("")
    R.dispatch_deflakes({}, dry=False)
    (tmp / "dispatch-paused").unlink()
    monkeypatch.setattr(R, "gate_holds_dispatch", lambda: True)
    R.dispatch_deflakes({}, dry=False)
    monkeypatch.setattr(R, "gate_holds_dispatch", lambda: False)
    monkeypatch.setattr(R, "disk_free_gb", lambda: 3.0)
    R.dispatch_deflakes({}, dry=False)
    assert launched == []
    monkeypatch.setattr(R, "disk_free_gb", lambda: 500.0)
    R.dispatch_deflakes({}, dry=False)
    assert len(launched) == 1


def test_busy_workers_counts_deflake_runs():
    claims = {"A": {"state": "running", "kind": "work"}, "DEFLAKE:x": {"state": "running", "kind": "deflake"},
              "DEFLAKE:y": {"state": "running", "kind": "review", "deflake": "y"}}
    assert R.busy_workers(claims) == 2


def test_launch_cuts_from_the_gated_base_and_runs_the_deflaker_bounded(df, monkeypatch):
    tmp, _, _, _ = df
    monkeypatch.undo()                                                # the real launch_deflake, tmp paths again
    for name, val in [("S", str(tmp)), ("WORKDIR", str(tmp / "work")), ("LOG", str(tmp / "work-runner.log")),
                      ("BULKMARK", str(tmp / "bulk-in-progress"))]:
        monkeypatch.setattr(R, name, val)
    (tmp / "bulk-in-progress").write_text("base=eab4bfae\nbranches=a\n")
    calls, popens = [], []
    monkeypatch.setattr(R, "sh", lambda args, cwd=R.REPO, timeout=120, check=False: calls.append(args) or "")

    class P:
        pid = 4242

    monkeypatch.setattr(R.subprocess, "Popen", lambda cmd, **kw: popens.append((cmd, kw)) or P())
    c = R.launch_deflake("deflake-a", _req("deflake-a", 7.0), {"run": 1}, dry=False)
    assert ["git", "worktree", "add", f"{R.REPO}/.claude/worktrees/deflake-a-r2", "-b", "task-deflake-a-r2", "eab4bfae"] in calls
    cmd, kw = popens[0]
    script = cmd[-1]
    assert "'--agent' 'deflaker'" in script and "'--model' 'opus'" in script and "'--effort' 'high'" in script
    assert "taskpolicy" in cmd and kw["start_new_session"] and kw["env"]["HK_WORKER"] == "1"
    assert kw["env"]["CARGO_BUILD_JOBS"] == R.WORKER_JOBS
    brief = (tmp / "work" / "deflake-a-r2" / "brief.md").read_text()
    for s in ("fog-of-war.e2e.mjs", "passed-alone", "merge-runner.log 09-23 10:00", "ALONE", "REAL BUG",
              "NEVER a retry", f"{tmp}/work/deflake-a-r2/handback.json", '"ticket": "DEFLAKE:deflake-a"', "HANDBACK: DONE"):
        assert s in brief, s
    assert c["ticket"] == "DEFLAKE:deflake-a" and c["kind"] == "deflake" and c["run"] == 2 and c["request_ts"] == 7.0
    assert c["base"] == "eab4bfae" and c["pid"] == 4242 and c["review"] is True


def test_deflake_claims_are_invisible_to_board_sync_and_candidates(df, monkeypatch):
    tmp, _, _, _ = df
    claims = {"DEFLAKE:deflake-a": {"ticket": "DEFLAKE:deflake-a", "deflake": "deflake-a", "state": "running",
                                    "kind": "deflake", "branch": "task-deflake-a", "group": None}}
    tasks = [{"id": "T-1", "status": "todo"}, {"id": "DEFLAKE:deflake-a", "status": "todo"}]  # even a colliding id
    monkeypatch.setattr(R, "main_safe_to_commit", lambda: True)
    monkeypatch.setattr(R, "board", lambda: [t for t in tasks if t["id"] == "T-1"])
    monkeypatch.setattr(R, "landed_tickets", lambda: {})
    R.sync_board(claims, dry=True)
    assert "would flip" not in (tmp / "work-runner.log").read_text() if (tmp / "work-runner.log").exists() else True
    monkeypatch.setattr(R, "sh", lambda args, cwd=R.REPO, timeout=120, check=False: "")
    assert [t["id"] for t in R.candidates(tasks[:1], claims)] == ["T-1"]
    assert R.release_stale_claims(dict(claims, **{"DEFLAKE:deflake-a": dict(claims["DEFLAKE:deflake-a"], state="no-work", started=0)}),
                                  {"T-1": tasks[0]}) is False    # never released as a "todo ticket"


@pytest.fixture
def reaped(df, monkeypatch):
    tmp, _, _, seen = df
    d = tmp / "work" / "deflake-a"
    d.mkdir(parents=True)
    monkeypatch.setattr(R, "alive", lambda pid: False)
    monkeypatch.setattr(R, "leaked_processes", lambda c, rows=None: [])
    monkeypatch.setattr(R, "sh", lambda args, cwd=R.REPO, timeout=120, check=False: "")
    reviews = []
    monkeypatch.setattr(R, "launch_review", lambda c: reviews.append(c) or dict(c, kind="review", pid=2))
    fixes = []
    monkeypatch.setattr(R, "launch_fix", lambda c, line: fixes.append(line) or c)

    def claim(kind="deflake", ahead=2):
        monkeypatch.setattr(R, "commits_ahead", lambda b, t: ahead)
        return {"DEFLAKE:deflake-a": {"ticket": "DEFLAKE:deflake-a", "deflake": "deflake-a", "test": "fog-of-war.e2e.mjs",
                                      "kind": kind, "state": "running", "pid": 1, "started": 0, "branch": "task-deflake-a",
                                      "wt": str(tmp / "nowt"), "dir": str(d), "review": True}}

    def hb(outcome, **kw):
        (d / "handback.json").write_text(json.dumps(dict({"ticket": "DEFLAKE:deflake-a", "outcome": outcome,
                                                          "summary": "rAF stopped in a hidden tab", "tests": []}, **kw)))
    return tmp, d, claim, hb, seen, reviews, fixes


def test_reap_done_with_commits_goes_to_review(reaped):
    tmp, d, claim, hb, seen, reviews, fixes = reaped
    hb("done", tests=[{"cmd": "node ui/e2e/run.mjs fog-of-war.e2e.mjs", "exit": 0}])
    claims = claim()
    R.reap(claims, dry=False)
    assert len(reviews) == 1 and claims["DEFLAKE:deflake-a"]["state"] == "running"
    assert claims["DEFLAKE:deflake-a"]["kind"] == "review" and seen == []
    assert json.loads((tmp / "work-done.jsonl").read_text())["outcome"] == "done-to-review"


def test_reap_without_commits_is_deflake_no_work(reaped):
    tmp, d, claim, hb, seen, reviews, fixes = reaped
    hb("done")
    claims = claim(ahead=0)
    R.reap(claims, dry=False)
    assert [k for _, k, _ in seen] == ["DEFLAKE_NO_WORK"] and reviews == []
    assert claims["DEFLAKE:deflake-a"]["state"] == "no-work" and claims["DEFLAKE:deflake-a"]["ended"]


def test_reap_blocked_carries_the_summary(reaped):
    tmp, d, claim, hb, seen, reviews, fixes = reaped
    hb("blocked", blocked={"needs": "fails alone 3/3: a real bug in the reveal path"})
    claims = claim(ahead=1)
    R.reap(claims, dry=False)
    (t, k, detail), = seen
    assert k == "DEFLAKE_BLOCKED" and "fails alone 3/3" in detail and "rAF stopped" in detail and reviews == []


def test_reap_done_over_a_failing_test_is_blocked(reaped):
    tmp, d, claim, hb, seen, reviews, fixes = reaped
    hb("done", tests=[{"cmd": "x", "exit": 1}])
    R.reap(claim(), dry=False)
    assert [k for _, k, _ in seen] == ["DEFLAKE_BLOCKED"] and reviews == []


def test_reap_the_fallback_line_and_the_bare_slug_are_accepted(reaped):
    tmp, d, claim, hb, seen, reviews, fixes = reaped
    (d / "out.json").write_text(json.dumps({"type": "result", "result": "...\nHANDBACK: BLOCKED fails alone"}))
    R.reap(claim(), dry=False)
    assert [k for _, k, _ in seen] == ["DEFLAKE_BLOCKED"]
    seen.clear()
    (d / "out.json").write_text("{}")
    hb("blocked", ticket="deflake-a", blocked={"needs": "slug-named"})
    R.reap(claim(), dry=False)
    assert "slug-named" in seen[0][2]


def test_review_pass_queues_and_review_fail_is_escalated_not_resumed(reaped):
    tmp, d, claim, hb, seen, reviews, fixes = reaped
    (d / "review.json").write_text(json.dumps({"result": "fine\nVERDICT: PASS"}))
    claims = claim(kind="review")
    R.reap(claims, dry=False)
    assert claims["DEFLAKE:deflake-a"]["state"] == "queued"
    assert (tmp / "merge-queue.txt").read_text().split() == ["task-deflake-a"]
    (d / "review.json").write_text(json.dumps({"result": "VERDICT: FAIL widened a timeout at x.mjs:10"}))
    claims = claim(kind="review")
    claims["DEFLAKE:deflake-a"]["session_id"] = "s"
    R.reap(claims, dry=False)
    assert [k for _, k, _ in seen] == ["DEFLAKE_REVIEW_FAIL"] and fixes == []
    R.handle_gate_failures(claims, dry=False)                          # the review-failed resume loop skips it too
    assert fixes == []


def test_a_gate_failure_on_a_deflake_branch_is_escalated_not_resumed(reaped):
    tmp, d, claim, hb, seen, reviews, fixes = reaped
    (tmp / "merge-needs-attention.txt").write_text("09-23 16:20  task-deflake-a  task-deflake-a  GATE_FAIL\n")
    claims = claim()
    claims["DEFLAKE:deflake-a"].update(state="queued", session_id="s")
    assert R.handle_gate_failures(claims, dry=False)
    assert fixes == [] and [k for _, k, _ in seen] == ["DEFLAKE_GATE_FAIL"]
    assert claims["DEFLAKE:deflake-a"]["state"] == "gate-failed"
    R.handle_gate_failures(claims, dry=False)
    assert len(seen) == 1
