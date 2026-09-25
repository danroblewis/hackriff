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
import time
import pathlib
import subprocess
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
    monkeypatch.setattr(R, "_run_fix", lambda c, n, prompt, fail_line="": seen.update(n=n, prompt=prompt) or c)
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


def test_only_a_branch_really_in_head_is_being_merged_and_it_waits(conflicts):
    """The marker's branches= lists every branch the batch ATTEMPTED - a skipped (conflicted) one
    too. Only a tip already in HEAD is being merged; that one waits (no fix run, line unseen), the
    skipped one gets its conflict run (T-848/T-849, 19:16 on 2026-09-23)."""
    import subprocess as sp
    tmp, launched = conflicts
    git = lambda *a: sp.run(["git", "-C", str(tmp), *a], check=True, capture_output=True)   # noqa: E731
    git("init", "-q", "-b", "main")
    git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "base")
    git("branch", "task-t849")                                            # skipped: not merged
    git("checkout", "-q", "-b", "task-t848")
    git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "work")
    git("checkout", "-q", "main")
    git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "main moved")
    git("checkout", "-q", "task-t849")
    git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "its work")
    git("checkout", "-q", "main")
    git("-c", "user.name=t", "-c", "user.email=t@t", "merge", "-q", "--no-ff", "-m", "batch merges t848", "task-t848")
    (tmp / "bulk-in-progress").write_text("base=abc\nbranches=task-t848 task-t849\n")
    assert R.merging_branches() == {"task-t848"}
    (tmp / "merge-needs-attention.txt").write_text(
        "09-23 19:16  task-t848  T-848  CONFLICT(skipped from bulk)\n09-23 19:16  task-t849  T-849  CONFLICT(skipped from bulk)\n")
    claims = {"T-848": _claim("T-848", "task-t848"), "T-849": _claim("T-849", "task-t849")}
    R.handle_gate_failures(claims, dry=False)
    assert "gate_fails_seen" not in claims["T-848"]                      # waits for its merge to end
    assert [t for t, _ in launched] == ["T-849"]                          # the skipped one is resumed

def test_sync_board_flag_runs_one_board_sync_and_exits(tmp_path, monkeypatch):
    (tmp_path / "claims.json").write_text('{"T-801": {"state": "no-work"}}')
    monkeypatch.setattr(R, "CLAIMS", str(tmp_path / "claims.json"))
    monkeypatch.setattr(R, "WORKDIR", str(tmp_path / "work"))
    seen = []
    monkeypatch.setattr(R, "sync_board", lambda claims, dry: seen.append((claims, dry)))

    def no_tick(dry):
        raise AssertionError("no tick, no dispatch")
    monkeypatch.setattr(R, "tick", no_tick)
    monkeypatch.setattr(sys, "argv", ["work-runner.py", "--sync-board"])
    R.main()
    assert seen == [({"T-801": {"state": "no-work"}}, False)]


def test_the_merge_runner_syncs_the_board_only_after_main_is_safe():
    text = (pathlib.Path(__file__).resolve().parents[2] / "ops" / "merge-runner.sh").read_text()
    bulk = text[text.index('log "BULK MERGED'):]
    assert bulk.index('rm -f "$BULKMARK"') < bulk.index("board_sync_now")     # marker gone first
    single = text[text.index('log "MERGED $branch'):]
    assert single.index("board_sync_now") < single.index("worktree_of")      # after the commit, at landing


def test_every_worker_gets_its_own_e2e_port_block_clear_of_the_gate():
    ports = {R.e2e_port_for(f"/x/.claude/worktrees/t{n}") for n in range(400, 900)}
    assert min(ports) >= 9216 and max(ports) + 256 <= 49152          # below the ephemeral range
    assert all((p - 9216) % 256 == 0 for p in ports)
    assert R.e2e_port_for("/x/.claude/worktrees/t845/") == R.e2e_port_for("/x/.claude/worktrees/t845")
    assert len(ports) > 120                                         # spread, not one block
    env = R.e2e_env("/x/.claude/worktrees/t845")
    base = int(env["HK_E2E_PORT"])
    assert int(env["HK_E2E_JOURNEY_PORT"]) == base + 224 and env["HK_E2E_CONCURRENCY"] == "3"
    assert base + 192 + 24 < base + 224 and base + 224 + 4 + 24 <= base + 256   # lanes and journey fit


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
                      ("BULKMARK", str(tmp_path / "bulk-in-progress")),
                      # Never the live .git/MERGE_HEAD: the merge gate runs these tests INSIDE a
                      # staged merge of main, where release_stale_claims correctly holds back (T-879).
                      ("REPO", str(tmp_path))]:
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


def test_a_request_waits_while_the_last_branch_is_unmerged_then_older_evidence_is_dropped(df, monkeypatch):
    tmp, write, launched, _ = df
    now = [10 * DAY]
    monkeypatch.setattr(R.time, "time", lambda: now[0])
    claims = {"DEFLAKE:deflake-a": {"ticket": "DEFLAKE:deflake-a", "deflake": "deflake-a", "kind": "review",
                                    "state": "queued", "branch": "task-deflake-a", "run": 1,
                                    "request_ts": now[0] - 2 * DAY, "ended": now[0] - 2 * 3600}}
    write(_req("deflake-a", now[0] - 2 * DAY), _req("deflake-a", now[0] - 60))   # a flake while the fix waits to land
    R.dispatch_deflakes(claims, dry=False)
    R.dispatch_deflakes(claims, dry=False)
    log = (tmp / "work-runner.log").read_text()
    assert launched == [] and log.count("DEFLAKE WAIT deflake-a") == 1 and "not merged yet" in log
    now[0] += 3600                                                     # the fix lands; the claim closes
    monkeypatch.setattr(R, "sh", lambda args, cwd=R.REPO, timeout=120, check=False:
                        "0" if args[:2] == ["git", "rev-list"] else "abc123")
    assert R.release_stale_claims(claims, {}) and claims["DEFLAKE:deflake-a"]["state"] == "merged"
    R.dispatch_deflakes(claims, dry=False)
    R.dispatch_deflakes(claims, dry=False)
    log = (tmp / "work-runner.log").read_text()
    assert launched == [] and log.count("DEFLAKE DROP deflake-a") == 1          # predates the fix: dropped, once
    assert claims["DEFLAKE:deflake-a"]["request_ts"] == now[0] - 3600 - 60
    write(_req("deflake-a", now[0] - 2 * DAY), _req("deflake-a", now[0] - 3600 - 60), _req("deflake-a", now[0] + 5))
    R.dispatch_deflakes(claims, dry=False)
    assert launched == [("deflake-a", now[0] + 5, 2)]                 # a flake AFTER the fix: run 2 at once


def test_a_finished_unmerged_run_does_not_hold_newer_evidence(df, monkeypatch):
    """No time window: a blocked / no-work run is over, so only its older evidence is dropped."""
    tmp, write, launched, _ = df
    claims = {"DEFLAKE:deflake-a": {"ticket": "DEFLAKE:deflake-a", "deflake": "deflake-a", "kind": "deflake",
                                    "state": "blocked", "run": 1, "request_ts": 100.0, "ended": 500.0}}
    write(_req("deflake-a", 100.0), _req("deflake-a", 400.0), _req("deflake-a", 600.0))
    R.dispatch_deflakes(claims, dry=False)
    assert launched == [("deflake-a", 600.0, 2)]
    assert "DEFLAKE DROP deflake-a: 1 request(s)" in (tmp / "work-runner.log").read_text()


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


def test_a_conflict_on_a_deflake_branch_takes_the_conflict_skip_path(reaped, monkeypatch):
    tmp, d, claim, hb, seen, reviews, fixes = reaped
    (tmp / "merge-needs-attention.txt").write_text("09-23 16:20  task-deflake-a  task-deflake-a  CONFLICT(skipped from bulk)\n")
    monkeypatch.setattr(R, "board_statuses", lambda: {"T-1": "todo"})   # no DEFLAKE key there: harmless
    monkeypatch.setattr(R, "_line_ts", lambda line: None)
    monkeypatch.setattr(R, "merging_branches", lambda: set())
    monkeypatch.setattr(R, "merges_cleanly", lambda b, target="main": True)
    claims = claim()
    claims["DEFLAKE:deflake-a"].update(state="queued", session_id="s")
    R.handle_gate_failures(claims, dry=False)
    assert (tmp / "merge-queue.txt").read_text().split() == ["task-deflake-a"]   # clean now: re-queued
    assert seen == [] and fixes == [] and claims["DEFLAKE:deflake-a"]["state"] == "queued"
    (tmp / "merge-needs-attention.txt").write_text("09-23 16:30  task-deflake-a  task-deflake-a  CONFLICT\n")
    monkeypatch.setattr(R, "merges_cleanly", lambda b, target="main": False)
    (tmp / "merge-queue.txt").write_text("")
    R.handle_gate_failures(claims, dry=False)
    assert fixes == [] and [k for _, k, _ in seen] == ["DEFLAKE_CONFLICT"]     # where a ticket gets launch_fix
    assert claims["DEFLAKE:deflake-a"]["state"] == "conflict" and claims["DEFLAKE:deflake-a"]["ended"]



def test_board_sync_runs_under_one_lock(tmp_path, monkeypatch):
    """The daemon and the merge runner's --sync-board both see main safe at one instant."""
    import fcntl
    monkeypatch.setattr(R, "S", str(tmp_path))
    held = []

    def inner(claims, dry):
        with open(tmp_path / "board-sync.lock", "a") as other:
            try:
                fcntl.flock(other, fcntl.LOCK_EX | fcntl.LOCK_NB)
                held.append(False)                       # we could take it: the wrapper did not hold it
            except OSError:
                held.append(True)
        return "synced"
    monkeypatch.setattr(R, "_sync_board", inner)
    assert R.sync_board({}, False) == "synced" and held == [True]



# --------------------------------------------------------------------- reaping stuck worktrees (2026-09-24)
def test_a_worktree_holding_only_build_output_is_reaped_and_real_files_are_kept_quietly(tmp_path, monkeypatch):
    """t356 (0 commits, claim blocked, only an untracked .githooks/) held 31 GB and failed
    `worktree remove` 1,499 times; a worktree whose directory was gone failed 1,730 times."""
    import os
    import shutil
    import subprocess as sp
    repo = tmp_path / "repo"
    repo.mkdir()
    g = lambda *a, cwd=repo: sp.run(["git", "-C", str(cwd), "-c", "user.name=t", "-c", "user.email=t@t", *a],  # noqa: E731
                                    check=True, capture_output=True, text=True)
    g("init", "-q", "-b", "main")
    g("commit", "-q", "--allow-empty", "-m", "base")
    wts = repo / ".claude" / "worktrees"
    for name in ("t356", "t900", "tgone"):
        g("worktree", "add", "-q", "-b", f"task-{name}", str(wts / name))
    (wts / "t356" / ".githooks").mkdir()
    (wts / "t356" / ".githooks" / "pre-commit").write_text("#!/bin/sh\n")
    (wts / "t356" / "target").mkdir()
    (wts / "t900" / "notes-i-never-committed.md").write_text("someone's work\n")
    shutil.rmtree(wts / "tgone")                                   # registered, directory gone
    old = 1_000_000_000
    for name in ("t356", "t900"):
        os.utime(wts / name, (old, old))

    real_sh = R.sh
    monkeypatch.setattr(R, "sh", lambda args, cwd=None, **k: real_sh(args, cwd=cwd or str(repo), **k))
    monkeypatch.setattr(R, "REPO", str(repo))
    monkeypatch.setattr(R, "BULKMARK", str(tmp_path / "no-bulk"))
    monkeypatch.setattr(R, "REAP_AFTER_MIN", 0)
    monkeypatch.setattr(R, "_REAP_SAID", set())
    said = []
    monkeypatch.setattr(R, "log", said.append)
    claims = {"T-356": {"state": "blocked", "wt": str(wts / "t356")}}

    R.reap_worktrees(claims, dry=False)
    assert not (wts / "t356").exists()                             # only build output: reaped
    assert (wts / "t900" / "notes-i-never-committed.md").exists()  # a real file: kept
    assert "tgone" not in g("worktree", "list").stdout             # pruned
    kept = [m for m in said if "kept - untracked" in m]
    assert len(kept) == 1 and "notes-i-never-committed.md" in kept[0]
    R.reap_worktrees(claims, dry=False)
    assert len([m for m in said if "kept - untracked" in m]) == 1  # said once, not every tick
    assert not any("fatal" in m for m in said)


@pytest.fixture
def killed_run(df, monkeypatch, tmp_path):
    """A ticket worker whose process is gone and left no out.json result and no handback.json."""
    monkeypatch.setenv("HK_ALERT_OFF", "1")
    wt = tmp_path / "wt-t802"
    wt.mkdir()
    (tmp_path / "work" / "T-802").mkdir(parents=True)
    (tmp_path / "work" / "T-802" / "out.json").write_text("")            # killed: claude never wrote its result
    monkeypatch.setattr(R, "alive", lambda pid: False)
    monkeypatch.setattr(R, "leaked_processes", lambda c, rows=None: [])
    monkeypatch.setattr(R, "sh", lambda args, cwd=R.REPO, timeout=120, check=False:
                        "2\n" if args[:3] == ["git", "rev-list", "--count"] else (" M ui/src/a.ts\n" if args[:2] == ["git", "status"] else ""))
    fixes, alerts = [], []
    monkeypatch.setattr(R, "launch_fix", lambda c, line: fixes.append(line) or dict(c, state="running", kind="fix"))
    monkeypatch.setattr(R, "alert", lambda *a: alerts.append(a))

    def claim(**kw):
        return {"T-802": dict({"ticket": "T-802", "branch": "task-t802", "wt": str(wt), "pid": 1, "started": 0,
                               "kind": "work", "state": "running", "model": "opus"}, **kw)}
    return claim, fixes, alerts, df[3]


def test_a_killed_worker_is_resumed_in_its_worktree(killed_run):
    """04:07 on 2026-09-24: a pkill took five workers; each was logged "NO_HANDBACK ... (done)", parked
    as uncommitted/no-work, and never ran again."""
    claim, fixes, alerts, seen = killed_run
    claims = claim(session_id="abc")
    R.reap(claims, dry=False)
    assert len(fixes) == 1 and fixes[0].startswith("KILLED") and "1 modified files and 2 commits" in fixes[0]
    assert claims["T-802"]["state"] == "running" and seen == []
    assert alerts[0][0] == "amber" and "T-802 (resumed)" in alerts[0][2]


def test_a_killed_worker_with_no_session_is_named_for_a_redispatch(killed_run):
    claim, fixes, alerts, seen = killed_run
    claims = claim()                                                       # launched before --session-id
    R.reap(claims, dry=False)
    assert fixes == [] and claims["T-802"]["state"] == "killed"
    assert [k for _, k, _ in seen] == ["KILLED"] and "T-802 (needs a redispatch)" in alerts[0][2]


def test_a_killed_resume_has_its_own_prompt_and_spends_no_fix_attempt(df, monkeypatch, tmp_path):
    """Review 2026-09-24: through the gate-failure prompt a killed worker would chase a gate that never
    ran, and a later real gate failure would get one fix attempt instead of two."""
    monkeypatch.setattr(R, "merge_target", lambda: "main")
    runs = []
    monkeypatch.setattr(R, "_run_fix", lambda c, n, prompt, out_name=None, fail_line="": runs.append((n, prompt, out_name)) or dict(c, state="running", fix_attempts=n))
    c = {"ticket": "T-802", "branch": "task-t802", "wt": str(tmp_path), "session_id": "abc", "fix_attempts": 1, "kind": "work"}
    r = R.launch_fix(c, "KILLED your run ended after 40 min with no result")
    (n, prompt, out_name), = runs
    assert n == 1 and r["fix_attempts"] == 1 and r["kill_resumes"] == 1 and out_name == "resume1.json"
    assert "KILLED from outside" in prompt and "merge gate" not in prompt and "merge-runner.log" not in prompt


def test_a_killed_claim_with_no_commits_is_released_like_no_work(df, monkeypatch):
    monkeypatch.setattr(R, "sh", lambda args, cwd=None, **k: "")
    claims = {"T-802": {"ticket": "T-802", "state": "killed", "started": 0, "branch": "task-t802"}}
    R.release_stale_claims(claims, {"T-802": {"id": "T-802", "status": "todo"}})
    assert "T-802" not in claims


def test_a_worker_that_wrote_its_result_is_not_killed(killed_run, tmp_path):
    claim, fixes, alerts, seen = killed_run
    (tmp_path / "work" / "T-802" / "out.json").write_text(json.dumps({"result": "done", "session_id": "abc"}))
    claims = claim(session_id="abc")
    R.reap(claims, dry=False)
    assert alerts == [] and not any("KILLED" in f for f in fixes)


def test_a_leaked_e2e_data_dir_is_removed_and_a_live_one_kept(tmp_path, monkeypatch):
    """2026-09-24 10:30: 49 hk-e2e-data-* dirs (89.6 GB) left by killed browser specs."""
    import os
    for n in ("leaked", "live", "fresh"):
        (tmp_path / f"hk-e2e-data-{n}").mkdir()
        (tmp_path / f"hk-e2e-data-{n}" / "ring.bin").write_text("x")
    (tmp_path / "unrelated").mkdir()
    old = 1_000_000_000
    for n in ("leaked", "live"):
        for p in (tmp_path / f"hk-e2e-data-{n}" / "ring.bin", tmp_path / f"hk-e2e-data-{n}"):
            os.utime(p, (old, old))
    os.utime(tmp_path / "unrelated", (old, old))
    monkeypatch.setattr(R.tempfile, "gettempdir", lambda: str(tmp_path))
    procs = f"/x/target/debug/hk serve --replay f --data-dir {tmp_path}/hk-e2e-data-live --ui-dist d\n"
    monkeypatch.setattr(R, "sh", lambda args, cwd=None, **k: procs)
    said = []
    monkeypatch.setattr(R, "log", said.append)
    R.reclaim_e2e_data(dry=True)
    assert (tmp_path / "hk-e2e-data-leaked").exists() and len(said) == 1
    R.reclaim_e2e_data(dry=False)
    assert sorted(p.name for p in tmp_path.iterdir()) == ["hk-e2e-data-fresh", "hk-e2e-data-live", "unrelated"]
    monkeypatch.setattr(R, "sh", lambda args, cwd=None, **k: "")          # ps said nothing: delete nothing
    for p in (tmp_path / "hk-e2e-data-fresh" / "ring.bin", tmp_path / "hk-e2e-data-fresh"):
        os.utime(p, (old, old))
    R.reclaim_e2e_data(dry=False)
    assert (tmp_path / "hk-e2e-data-fresh").exists()


def test_an_idle_target_of_a_kept_worktree_is_reclaimed(tmp_path, monkeypatch):
    """09-24 09:47: 55 GB of build output sat in twelve worktrees the reaper keeps (timeout, blocked,
    uncommitted); free disk was 22 GB against a 20 GB dispatch floor."""
    import os
    root = tmp_path / ".claude" / "worktrees"
    old = 1_000_000_000
    for name in ("idle", "fresh", "inuse", "claimed", "t87"):
        (root / name / "target" / "debug").mkdir(parents=True)
        (root / name / "src.rs").write_text("kept\n")
    (root / "linked").mkdir()
    (root / "linked" / "target").symlink_to(root / "idle" / "target")
    monkeypatch.setattr(R, "_target_written", lambda t: time.time() if "/fresh/" in t else old)
    monkeypatch.setattr(R, "REPO", str(tmp_path))
    procs = f"node {root}/inuse/ui/e2e/run.mjs\n"                 # a process in inuse; none in t87 (t870 is a prefix trap)
    lsof = f"p1\nn{root}/t870\n"
    monkeypatch.setattr(R, "sh", lambda args, cwd=None, **k: procs if args[0] == "ps" else lsof)
    said = []
    monkeypatch.setattr(R, "log", said.append)
    claims = {"T-1": {"state": "running", "wt": str(root / "claimed")}}
    R.reclaim_idle_targets(claims, dry=True)
    assert all((root / n / "target").exists() for n in ("idle", "fresh", "inuse", "claimed", "t87"))
    R.reclaim_idle_targets(claims, dry=False)
    gone = sorted(n for n in ("idle", "fresh", "inuse", "claimed", "t87") if not (root / n / "target").exists())
    assert gone == ["idle", "t87"]
    assert all((root / n / "src.rs").exists() for n in ("idle", "t87"))   # the source is never touched
    assert len([m for m in said if m.startswith("RECLAIM")]) == 2
    assert (root / "linked").is_symlink() is False and os.path.islink(root / "linked" / "target")


def test_an_idle_target_is_kept_when_lsof_says_nothing(tmp_path, monkeypatch):
    root = tmp_path / ".claude" / "worktrees"
    (root / "idle" / "target" / "debug").mkdir(parents=True)
    monkeypatch.setattr(R, "_target_written", lambda t: 1_000_000_000)
    monkeypatch.setattr(R, "REPO", str(tmp_path))
    monkeypatch.setattr(R, "sh", lambda args, cwd=None, **k: "")
    monkeypatch.setattr(R, "log", lambda m: None)
    R.reclaim_idle_targets({}, dry=False)
    assert (root / "idle" / "target").exists()


def test_a_target_cloned_with_old_mtimes_reads_as_just_written(tmp_path):
    """cp -c -R -p keeps main's mtimes; the clone's ctime is when it happened (review, 09-24)."""
    import os
    t = tmp_path / "target"
    (t / "debug" / "deps").mkdir(parents=True)
    for p in (t / "debug" / "deps", t / "debug", t):
        os.utime(p, (1_000_000_000, 1_000_000_000))          # an old mtime, as `cp -p` leaves it
    assert time.time() - R._target_written(str(t)) < 60


def test_only_regenerable_is_strict():
    assert R.only_regenerable([".githooks/", "target/"])
    assert not R.only_regenerable([".githooks/", "src/new.rs"])
    assert not R.only_regenerable([])


def test_a_deflake_waits_while_a_ticket_branch_edits_its_spec(df, monkeypatch):
    """2026-09-23 23:30: the runner dispatched deflakers on app-trace and fog-of-war while T-801's
    worker was rewriting both under the user's authorization; the coordinator held one by hand."""
    tmp, write, launched, _ = df
    write(_req("deflake-a", 100.0, test="app-trace.e2e.mjs"))
    edits = {"task-t801": "ui/e2e/app-trace.e2e.mjs\n"}
    monkeypatch.setattr(R, "sh", lambda args, cwd=R.REPO, timeout=120, check=False:
                        edits.get(args[3].split("...")[1], "") if args[:3] == ["git", "diff", "--name-only"] else "")
    claims = {"T-801": {"ticket": "T-801", "branch": "task-t801", "state": "review-failed", "kind": "work"},
              "T-9": {"ticket": "T-9", "branch": "task-t9", "state": "running", "kind": "work"}}
    R.dispatch_deflakes(claims, dry=False)
    R.dispatch_deflakes(claims, dry=False)
    assert launched == []
    assert (tmp / "work-runner.log").read_text().count("DEFLAKE WAIT deflake-a: request for app-trace.e2e.mjs - T-801's") == 1
    claims["T-801"]["state"] = "queued"                   # a queued claim whose branch is in no queue: stale
    R.dispatch_deflakes(claims, dry=False)
    assert [s for s, _, _ in launched] == ["deflake-a"]
    launched.clear()
    (tmp / "merge-queue.txt").write_text("task-t801\n")    # ...but really queued: it waits
    claims["DEFLAKE:deflake-a"]["state"] = "no-work"
    write(_req("deflake-a", 100.0, test="app-trace.e2e.mjs"), _req("deflake-a", 2e9, test="app-trace.e2e.mjs"))
    R.dispatch_deflakes(claims, dry=False)
    assert launched == []
    edits.clear()                                        # landed: main now has it, the three-dot diff is empty
    R.dispatch_deflakes(claims, dry=False)
    assert [s for s, _, _ in launched] == ["deflake-a"]


def test_a_deflake_waits_while_its_own_last_branch_is_unmerged(df, monkeypatch):
    tmp, write, launched, _ = df
    write(_req("deflake-a", 600.0))
    monkeypatch.setattr(R, "commits_ahead", lambda b, t: 1)
    claims = {"DEFLAKE:deflake-a": {"ticket": "DEFLAKE:deflake-a", "deflake": "deflake-a", "kind": "deflake", "state": "blocked",
                                    "branch": "task-deflake-a", "run": 1, "request_ts": 100.0, "ended": 500.0}}
    R.dispatch_deflakes(claims, dry=False)
    assert launched == [] and "has unmerged commits" in (tmp / "work-runner.log").read_text()
    monkeypatch.setattr(R, "commits_ahead", lambda b, t: 0)
    R.dispatch_deflakes(claims, dry=False)
    assert launched == [("deflake-a", 600.0, 2)]


def test_a_deflake_waits_while_its_last_branch_gates_in_a_batch(df, monkeypatch):
    """09-24 09:39:52: main held the batch carrying task-deflake-app-trace-e2e-mjs, so the branch read
    0 commits ahead of main and a second deflaker was dispatched beside its own gating fix."""
    tmp, write, launched, _ = df
    monkeypatch.setattr(R, "_DEFER_SAID", set())         # module-global: the test above said this WAIT
    write(_req("deflake-a", 600.0))
    (tmp / "bulk-in-progress").write_text("base=gatedbase\nbranches=task-deflake-a\n")
    monkeypatch.setattr(R, "commits_ahead", lambda b, t: 0 if t == "main" else 3)
    claims = {"DEFLAKE:deflake-a": {"ticket": "DEFLAKE:deflake-a", "deflake": "deflake-a", "kind": "deflake", "state": "blocked",
                                    "branch": "task-deflake-a", "run": 1, "request_ts": 100.0, "ended": 500.0}}
    R.dispatch_deflakes(claims, dry=False)
    assert launched == [] and "has unmerged commits" in (tmp / "work-runner.log").read_text()
    (tmp / "bulk-in-progress").unlink()                  # the batch landed: main is gated again
    R.dispatch_deflakes(claims, dry=False)
    assert launched == [("deflake-a", 600.0, 2)]


def test_a_red_proof_is_not_a_failing_test(reaped):
    tmp, d, claim, hb, seen, reviews, fixes = reaped
    hb("done", tests=[{"cmd": "node run.mjs app-trace.e2e.mjs  # defect injected", "exit": 1, "expect": "red"},
                      {"cmd": "node run.mjs app-trace.e2e.mjs", "exit": 0}])
    claims = claim()
    R.reap(claims, dry=False)
    assert len(reviews) == 1 and seen == []
    hb("done", tests=[{"cmd": "x", "exit": 1, "expect": "red"}])     # a "proof" with no green run beside it
    R.reap(claim(), dry=False)
    assert [k for _, k, _ in seen] == ["DEFLAKE_BLOCKED"]


def test_a_claim_whose_ticket_landed_as_a_rebuilt_branch_is_closed(tmp_path, monkeypatch):
    """task-t538 landed as task-t538-rl: its own branch is never on main, so the claim stayed
    `queued` for 42 h (12 of 13 such on 2026-09-24). Board done + in no queue = closed."""
    monkeypatch.setattr(R, "MERGE_QUEUE", str(tmp_path / "merge-queue.txt"))
    monkeypatch.setattr(R, "BULKMARK", str(tmp_path / "bulk-in-progress"))
    monkeypatch.setattr(R, "LOG", str(tmp_path / "work-runner.log"))
    monkeypatch.setattr(R, "REPO", str(tmp_path))                     # never the live .git/MERGE_HEAD (T-879)
    monkeypatch.setattr(R, "sh", lambda args, cwd=R.REPO, timeout=120, check=False: "")     # branch not on main
    (tmp_path / "merge-queue.txt").write_text("task-t2\n")
    (tmp_path / "bulk-in-progress").write_text("base=abc\nbranches=task-t3 task-t4\n")
    claims = {t: {"ticket": t, "branch": f"task-t{t[2:]}", "state": "queued", "started": 0} for t in ("T-1", "T-2", "T-3", "T-5")}
    board = {"T-1": {"status": "done"}, "T-2": {"status": "done"}, "T-3": {"status": "done"}, "T-5": {"status": "in-progress"}}
    assert R.release_stale_claims(claims, board)
    assert {t: c["state"] for t, c in claims.items()} == {"T-1": "merged", "T-2": "queued", "T-3": "queued", "T-5": "queued"}
    assert "CLAIM T-1: the board says done and task-t1 is in no queue" in (tmp_path / "work-runner.log").read_text()


def test_no_rebuilt_branch_close_while_a_single_merge_is_staged(tmp_path, monkeypatch):
    monkeypatch.setattr(R, "MERGE_QUEUE", str(tmp_path / "merge-queue.txt"))
    monkeypatch.setattr(R, "BULKMARK", str(tmp_path / "bulk-in-progress"))
    monkeypatch.setattr(R, "LOG", str(tmp_path / "work-runner.log"))
    monkeypatch.setattr(R, "REPO", str(tmp_path))
    monkeypatch.setattr(R, "sh", lambda args, cwd=R.REPO, timeout=120, check=False: "")
    (tmp_path / ".git").mkdir()
    (tmp_path / ".git" / "MERGE_HEAD").write_text("abc\n")      # the runner is merging task-t1 alone
    claims = {"T-1": {"ticket": "T-1", "branch": "task-t1", "state": "queued", "started": 0}}
    R.release_stale_claims(claims, {"T-1": {"status": "done"}})
    assert claims["T-1"]["state"] == "queued"
def test_the_runner_drops_an_inherited_role_before_it_starts_a_worker():
    """2026-09-24 03:20: restarted from the pipeline-manager session, the runner passed
    HACKRIFF_ROLE=pipeline-manager to every worker, and the watchdog charged their 665 % to that role."""
    src = _WR.read_text()
    main_body = src[src.index("def main():"):]
    pop = main_body.index('os.environ.pop("HACKRIFF_ROLE", None)')
    assert pop < main_body.index("while True") and pop < main_body.index('log(f"VERSION:')


def test_no_claim_is_closed_as_on_main_while_main_is_provisional(tmp_path, monkeypatch):
    """T-866, 2026-09-24 03:33:46: closed as "on main" because the batch had committed its merge before
    gating; the batch failed 16 s later and the red had no claim for the work runner to resume."""
    monkeypatch.setattr(R, "MERGE_QUEUE", str(tmp_path / "merge-queue.txt"))
    monkeypatch.setattr(R, "BULKMARK", str(tmp_path / "bulk-in-progress"))
    monkeypatch.setattr(R, "LOG", str(tmp_path / "work-runner.log"))
    monkeypatch.setattr(R, "REPO", str(tmp_path))
    monkeypatch.setattr(R, "sh", lambda args, cwd=R.REPO, timeout=120, check=False:
                        "0" if args[:2] == ["git", "rev-list"] else "abc123")          # the branch reads as on main
    (tmp_path / "bulk-in-progress").write_text("base=abc\nbranches=task-t866\n")
    claims = {"T-866": {"ticket": "T-866", "branch": "task-t866", "state": "queued", "started": 0}}
    R.release_stale_claims(claims, {"T-866": {"status": "in-progress"}})
    assert claims["T-866"]["state"] == "queued"
    (tmp_path / "bulk-in-progress").unlink()                                             # the batch landed
    R.release_stale_claims(claims, {"T-866": {"status": "in-progress"}})
    assert claims["T-866"]["state"] == "merged"


def test_needs_a_person_titles_only_what_no_automation_picks_up(tmp_path, monkeypatch):
    """User, 2026-09-24 11:40: six "needs a person" alerts were fix-run outcomes; the user came asking
    what to decide. The title is reserved for escalations, cancellations, unresolved review FAILs and
    BLOCKED hand-backs that ask for a decision."""
    monkeypatch.setattr(R, "NEEDS", str(tmp_path / "needs.txt"))
    monkeypatch.setattr(R, "LOG", str(tmp_path / "log"))
    monkeypatch.delenv("PYTEST_CURRENT_TEST", raising=False)
    sent = []

    class Ok:
        returncode = 1                                                   # no tmux session: no send-keys
    monkeypatch.setattr(R.subprocess, "run", lambda args, **k: (sent.append(args) if "alert.py" in " ".join(args) else None) or Ok())
    for kind, detail in [("CONFLICT_ESCALATE", "2 fix attempts spent"), ("CANCEL_PROPOSED", "already done"),
                         ("BLOCKED", "User decision per use case"), ("BLOCKED", "fails alone: real bug in hk-api"),
                         ("ERROR", "claude -p error"), ("NO_WORK", "no commits")]:
        R.attention("T-9", "task-t9", kind, detail)
    titles = [a[next(i for i, x in enumerate(a) if x.endswith("alert.py")) + 2] for a in sent if "--no-receiver" not in a]
    assert titles == ["needs a person - T-9 CONFLICT_ESCALATE", "needs a person - T-9 CANCEL_PROPOSED",
                      "needs a person - T-9 BLOCKED", "T-9 BLOCKED", "T-9 ERROR"]


def test_the_merge_runner_says_needs_a_person_only_when_it_gave_up():
    text = (pathlib.Path(__file__).resolve().parents[2] / "ops" / "merge-runner.sh").read_text()
    calls = [ln for ln in text.splitlines() if "notify_coordinator \"" in ln]
    assert calls and all(ln.rstrip().rstrip(";").split('"')[-2] for ln in calls)     # every call titles itself
    person = [ln for ln in calls if "needs a person" in ln]
    assert len(person) == 1 and "GIVEN UP" in person[0]
    gate_fail = next(ln for ln in calls if "FAILED the merge gate" in ln)
    assert '"gate failed - fix run"' in gate_fail and "TRIAGE_SPECS" in gate_fail   # names the failing test/spec


def test_the_reserve_cap_holds_through_the_gap_between_two_gates(tmp_path, monkeypatch):
    """2026-09-24 14:30: an isolation's single gates leave 3-8 s gaps with no marker; each gap a tick
    landed in dispatched up to WORK_CAP and the next gate ran beside 7 workers (load 58 vs plan 32).
    Not keyed on the queue (review): held/parked branches sit there with no gate coming."""
    monkeypatch.setattr(R, "MERGE_QUEUE", str(tmp_path / "merge-queue.txt"))
    monkeypatch.setattr(R, "BULKMARK", str(tmp_path / "bulk-in-progress"))
    monkeypatch.setattr(R, "REPO", str(tmp_path))
    monkeypatch.setattr(R, "S", str(tmp_path))
    monkeypatch.setattr(R, "GATE_ALONE", False)
    monkeypatch.setattr(R, "CAP", 6)
    monkeypatch.setattr(R, "RESERVE_CAP", 4)
    monkeypatch.setattr(R, "_GATE_SEEN", [0.0])
    clock = [1_000_000.0]
    monkeypatch.setattr(R.time, "time", lambda: clock[0])
    (tmp_path / "merge-queue.txt").write_text("task-parked\n")
    assert R.dispatch_cap() == 6                                          # queued but no gate: full cap
    (tmp_path / "bulk-in-progress").write_text("base=abc\n")
    assert R.dispatch_cap() == 4                                          # a gate runs
    (tmp_path / "bulk-in-progress").unlink()
    clock[0] += 20
    assert R.dispatch_cap() == 4                                          # the gap before the next gate
    clock[0] += 100
    assert R.dispatch_cap() == 6                                          # the pipeline went quiet


def test_a_red_that_is_not_the_workers_own_queues_instead_of_blocking(monkeypatch):
    """Supervisor, 2026-09-24 18:55: T-809 read BLOCKED 'needs a person' for app-surface failing the same
    way on main's build at load 44 - the gate and its flake triage are the arbiter of such a red."""
    class E:
        def __init__(self, n): self.passed_alone = n
    import types
    fake = types.SimpleNamespace(ledger=lambda ops: {"app-surface.e2e.mjs": E(2), "app-sheet.e2e.mjs": E(1), "canvas.e2e.mjs": E(0)})
    monkeypatch.setitem(__import__("sys").modules, "hkpy.flakes", fake)
    monkeypatch.setattr(__import__("hkpy"), "flakes", fake, raising=False)
    assert R.not_own_red({"cmd": "cargo nextest run -p hk-x", "exit": 1, "reproduces_on_main": True})
    assert R.not_own_red({"cmd": "x", "exit": 1, "known_flake": True})
    assert R.not_own_red({"cmd": "cd ui && node e2e/run.mjs app-surface app-sheet", "exit": 1})          # ledger-known flakers
    assert not R.not_own_red({"cmd": "cd ui && node e2e/run.mjs app-surface app-detail", "exit": 1})     # app-detail unknown
    assert not R.not_own_red({"cmd": "cd ui && node e2e/run.mjs canvas", "exit": 1})                     # never passed alone
    assert not R.not_own_red({"cmd": "cargo nextest run -p hk-x", "exit": 101})                          # a plain red: blocked


def test_work_clone_target_0_launches_without_a_target_clone(monkeypatch):
    """2026-09-24 18:11: 83 GB of main's target/ still shared with three worker clones against 101 GB
    free - WORK_CLONE_TARGET=0 stops new pins; the worker builds from sccache."""
    monkeypatch.setattr(R, "CLONE_TARGET", True)
    assert "cp -c -R -p" in R.clone_cmd("/w/t1")
    monkeypatch.setattr(R, "CLONE_TARGET", False)
    assert R.clone_cmd("/w/t1") == ""


def test_the_queue_depth_is_sampled_once_a_minute(tmp_path, monkeypatch):
    """User, 2026-09-24 17:02: track 'branches not yet on main' on /flow; the work runner samples it
    (it ticks through a gate; the merge runner's loop does not)."""
    monkeypatch.setattr(R, "S", str(tmp_path))
    monkeypatch.setattr(R, "_DEPTH_AT", [0.0])
    (tmp_path / "merge-queue.txt").write_text("task-a\ntask-b\n")
    (tmp_path / "isolate-remaining").write_text("task-c\n")
    R.record_queue_depth()
    R.record_queue_depth()                                               # inside the minute: nothing
    (line,) = (tmp_path / "queue-depth.jsonl").read_text().splitlines()
    rec = json.loads(line)
    assert rec["waiting"] == 3 and rec["queued"] == 2 and rec["isolating"] == 1


def test_the_merge_runner_writes_what_it_holds_in_memory():
    text = (pathlib.Path(__file__).resolve().parents[2] / "ops" / "merge-runner.sh").read_text()
    assert 'echo "$1" > "$S/merging-now"; process "$1"' in text and 'rm -f "$S/merging-now"' in text
    loop = text[text.index('        rest="$isolate"'):]
    assert '> "$S/isolate-remaining"' in loop[:400] and 'rm -f "$S/isolate-remaining"' in loop[:1200]


def test_a_restart_requeues_what_a_killed_isolation_or_merge_was_holding(tmp_path):
    """Review 2026-09-24: a runner killed mid-isolation or mid single merge left those branches in no
    queue (the reason a restart during an isolation lost them) and now a stale depth file."""
    text = (pathlib.Path(__file__).resolve().parents[2] / "ops" / "merge-runner.sh").read_text()
    i = text.index('for f in "$S/isolate-remaining" "$S/merging-now"; do')
    block = text[i:text.index("\ndone\n", i) + 6]
    (tmp_path / "isolate-remaining").write_text("task-b task-c task-d\n")
    (tmp_path / "merging-now").write_text("task-a\n")
    (tmp_path / "q").write_text("task-x\n")
    script = f'set -u\nS={tmp_path}; QUEUE={tmp_path}/q\nlog(){{ echo "LOG $*"; }}\n{block}\n'
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30)
    assert out.returncode == 0, out.stderr
    assert (tmp_path / "q").read_text().split() == ["task-x", "task-b", "task-c", "task-d", "task-a"]
    assert not (tmp_path / "isolate-remaining").exists() and not (tmp_path / "merging-now").exists()


def test_the_sweep_keeps_the_newest_executable_per_build_unit_everywhere_after_a_landing(tmp_path, monkeypatch):
    """Supervisor, 2026-09-24 21:14: 2365 superseded test executables (~40 GB apparent) sat in main's target/,
    gate-target and every worker clone - the same blocks, freed only when the last copy goes. A unit is cargo's
    (package, target kind), never the file stem: `hk` is hk-cli's bin AND its test harness, and `review_fixes`
    is an integration test in two crates (review: the stem-keyed first pass deleted current twins)."""
    import os
    repo, ops = tmp_path / "repo", tmp_path / "ops"

    def profile(base):
        (base / "debug" / "deps").mkdir(parents=True)
        return base / "debug"

    def exe(prof, pkg, kind, stem, h, mtime, mode=0o755, profile=1):
        fp = prof / ".fingerprint" / f"{pkg}-{h}"
        fp.mkdir(parents=True)
        (fp / kind).write_text("")
        (fp / f"{kind}.json").write_text(json.dumps({"profile": profile, "features": "[]", "rustflags": []}))
        (fp / "invoked.timestamp").write_text("")
        p = prof / "deps" / f"{stem}-{h}"
        p.write_text("x")
        p.chmod(mode)
        os.utime(p, (mtime, mtime))
        return p

    def units(prof):
        return {
            "old": exe(prof, "hk-cli", "test-integration-test-api_contract", "api_contract", "00000000000000a1", 100),
            "new": exe(prof, "hk-cli", "test-integration-test-api_contract", "api_contract", "00000000000000a2", 200),
            "bin": exe(prof, "hk-cli", "bin-hk", "hk", "00000000000000b1", 300),            # same stem,
            "harness": exe(prof, "hk-cli", "test-bin-hk", "hk", "00000000000000b2", 100),   # different units
            "rf1": exe(prof, "hk-plugins", "test-integration-test-review_fixes", "review_fixes", "00000000000000c1", 50),
            "rf2": exe(prof, "hk-stream", "test-integration-test-review_fixes", "review_fixes", "00000000000000c2", 90),
            # the same unit in the workers' line-tables-only profile: current too, never "superseded"
            "lt": exe(prof, "hk-cli", "test-integration-test-api_contract", "api_contract", "00000000000000e1", 50, profile=2),
        }
    main_p, gate_p = profile(repo / "target"), profile(ops / "gate-target")
    busy_p, idle_p = profile(repo / ".claude/worktrees/t1/target"), profile(repo / ".claude/worktrees/t2/target")
    made = {prof: units(prof) for prof in (main_p, gate_p, busy_p, idle_p)}
    orphan = gate_p / "deps" / "mystery-00000000000000d1"                               # no fingerprint: kept
    orphan.write_text("x")
    orphan.chmod(0o755)
    held = made[idle_p]["old"]
    monkeypatch.setattr(R, "REPO", str(repo))
    monkeypatch.setattr(R, "S", str(ops))
    monkeypatch.setattr(R, "LOG", str(tmp_path / "log"))
    monkeypatch.setattr(R, "BULKMARK", str(ops / "bulk-in-progress"))
    monkeypatch.setattr(R, "disk_free_gb", lambda: 0.0)
    wt1 = str(repo / ".claude/worktrees/t1")
    obj = gate_p / "deps" / "api_contract-00000000000000a1.api_contract.abc123-cgu.0.rcgu.o"   # split debuginfo
    obj.write_text("o")
    lsof = {"stderr": ""}

    class Run:
        def __init__(self, stdout, stderr):
            self.stdout, self.stderr = stdout, stderr

    def run(args, **kw):
        assert args[:2] == ["lsof", "-Fn"]
        return Run(f"p9\nn{held}\n" if str(held.parent) in args else "", lsof["stderr"])
    monkeypatch.setattr(R.subprocess, "run", run)

    def sh(args, cwd=None, timeout=120, check=False):
        if args[:2] == ["git", "-C"]:
            return "merge1\n"
        if "-d" in args and "cwd" in args:          # cargo in t1's tree; stage's release cargo in main's checkout
            return f"p1\nn{wt1}/crates/hk-core\np2\nn{repo}\n"
        if args[:2] == ["ps", "eww"]:
            return f"cargo build CARGO_TARGET_DIR={ops}/target-serve" if args[-1] == "2" else "cargo test"
        return ""
    monkeypatch.setattr(R, "sh", sh)
    (ops / "bulk-in-progress").write_text("base=x\n")
    R.sweep_superseded(dry=False)
    assert all(p.exists() for u in made.values() for p in u.values())            # never during a gate
    (ops / "bulk-in-progress").unlink()
    lsof["stderr"] = "lsof: WARNING: can't stat() directory"                     # lsof failed: nothing deleted
    R.sweep_superseded(dry=False)
    assert all(p.exists() for u in made.values() for p in u.values())
    assert not (ops / "sweep-last").exists()
    lsof["stderr"] = ""
    R.sweep_superseded(dry=False)
    for prof in (main_p, gate_p):                    # main swept: stage's cargo writes to target-serve
        u = made[prof]
        assert not u["old"].exists() and u["new"].exists()
        assert u["bin"].exists() and u["harness"].exists() and u["rf1"].exists() and u["rf2"].exists()
        assert u["lt"].exists()                      # the other profile's build of the same unit is current
    assert not obj.exists()                          # the swept executable's .rcgu.o go with it
    assert held.exists() and made[idle_p]["new"].exists()                        # open in a process: kept
    assert all(p.exists() for p in made[busy_p].values())                        # a cargo works there: skipped
    assert orphan.exists()
    assert (ops / "sweep-last").read_text().strip() == "merge1"
    made[gate_p]["old"].write_text("x")
    made[gate_p]["old"].chmod(0o755)
    R.sweep_superseded(dry=False)
    assert made[gate_p]["old"].exists()                                          # once per landing, not every tick


def test_a_workers_red_proof_beside_a_green_run_is_not_a_failing_test():
    """2026-09-24: T-894 (15:25) and T-905 (20:16) handed back DONE with their new test's red run on the old
    code listed at exit 1, and read BLOCKED 'needs a person'. "expect": "red" + a green run = evidence."""
    proof = {"cmd": "cd ui && node test/run.mjs surface-survey (on old code)", "exit": 1, "expect": "red"}
    fixed = {"cmd": "cd ui && node test/run.mjs surface-survey", "exit": 0}
    assert R.hand_back_reds({"tests": [proof, fixed]}) == ([], [])
    assert R.hand_back_reds({"tests": [proof]}) == ([proof], [proof])            # no green run beside it
    plain = {"cmd": "cargo nextest run -p hk-x", "exit": 101}
    assert R.hand_back_reds({"tests": [plain, fixed]}) == ([plain], [plain])     # an unmarked red still blocks
    assert R.hand_back_reds(None) == ([], [])


def test_a_timed_out_worker_is_resumed_once_to_wrap_up(killed_run, monkeypatch):
    """2026-09-24: T-852, T-878, T-888, T-887 and T-904 each hit the 180-min limit, were parked 'worktree kept'
    and finished by hand. One wrap-up resume of the same session first; a second limit is a person's."""
    claim, fixes, alerts, seen = killed_run
    state = {"alive": True}
    monkeypatch.setattr(R, "alive", lambda pid: state["alive"])
    monkeypatch.setattr(R, "track_usage", lambda c: False)
    monkeypatch.setattr(R.os, "killpg", lambda pid, sig: state.update(alive=False))
    monkeypatch.setattr(R, "_gone", lambda pid: True)                      # the real one: the test below
    claims = claim(session_id="abc", started=R.time.time() - (R.MAX_MINUTES + 5) * 60)
    R.reap(claims, dry=False)
    assert len(fixes) == 1 and fixes[0].startswith("TIMEOUT") and seen == []
    state["alive"] = True                                                  # the wrap-up runs out of time too
    claims = claim(session_id="abc", kind="fix", timeout_resumes=1, started=R.time.time() - (R.MAX_MINUTES + 5) * 60)
    R.reap(claims, dry=False)
    assert len(fixes) == 1 and [k for _, k, _ in seen] == ["TIMEOUT"]


def test_the_wrap_up_resume_has_its_own_prompt_and_spends_no_fix_attempt(df, monkeypatch):
    runs = []
    monkeypatch.setattr(R, "_run_fix", lambda c, n, prompt, out_name=None, fail_line="": runs.append((n, prompt, out_name)) or dict(c, state="running"))
    monkeypatch.setattr(R, "gate_holds_dispatch", lambda: False)
    c = R.launch_fix({"ticket": "T-9", "branch": "task-t9", "wt": "/w/t9", "session_id": "s", "fix_attempts": 0},
                     "TIMEOUT your run reached the 180-min limit and was stopped")
    n, prompt, out = runs[0]
    assert n == 0 and out == "wrapup1.json" and c["timeout_resumes"] == 1
    assert "WRAP UP" in prompt and "Start no new scope" in prompt and "blocked.needs" in prompt


def test_a_stopped_runs_group_is_seen_gone_though_its_leader_is_our_unreaped_child():
    """Review, 2026-09-24: os.kill(pid, 0) succeeds on a zombie, and the runner never waits on its Popen
    children - the resume never fired and each timeout stalled the tick 31 s. A real child, dropped unwaited."""
    import signal
    p = subprocess.Popen(["sh", "-c", "sleep 60 & sleep 60"], start_new_session=True)
    pid = p.pid
    del p
    R.os.killpg(pid, signal.SIGTERM)
    t0 = time.time()
    assert R._gone(pid, wait_s=10) and time.time() - t0 < 5


@pytest.fixture
def remote_host(tmp_path, monkeypatch):
    """A 'remote' host that is this machine: remote commands run in a local bash (with a setsid shim - macOS has
    none), and the host's roots are distinct from the local ones so the path translation is exercised."""
    import os
    local_ops, local_repo, far = tmp_path / "ops", tmp_path / "repo", tmp_path / "far"
    for d in (local_ops / "work", local_repo, far / "ops" / "work", far / "repo"):
        d.mkdir(parents=True)
    shim = tmp_path / "bin"
    shim.mkdir()
    (shim / "setsid").write_text("#!/usr/bin/env python3\nimport os, sys\nos.setsid()\nos.execvp(sys.argv[1], sys.argv[1:])\n")
    (shim / "setsid").chmod(0o755)
    # the host has GNU tee (-p); this machine may have BSD tee, which has none
    (shim / "tee").write_text('#!/bin/bash\nargs=(); for a in "$@"; do [ "$a" = -p ] || args+=("$a"); done; exec /usr/bin/tee "${args[@]}"\n')
    (shim / "tee").chmod(0o755)
    monkeypatch.setenv("PATH", f"{shim}:{os.environ['PATH']}")
    monkeypatch.setattr(R, "S", str(local_ops))
    monkeypatch.setattr(R, "REPO", str(local_repo))
    monkeypatch.setattr(R, "WORKDIR", str(local_ops / "work"))
    monkeypatch.setattr(R, "LOG", str(tmp_path / "log"))
    monkeypatch.setattr(R, "HOSTS_FILE", str(local_ops / "hosts.json"))
    (local_ops / "hosts.json").write_text(json.dumps({"node2": {"ssh": "u@h", "repo": str(far / "repo"), "ops": str(far / "ops")}}))

    def remote_sh(host, cmd, timeout=120, input=None):
        r = subprocess.run(["bash", "-c", R.to_remote(host, cmd)], input=input, capture_output=True, text=True, timeout=timeout)
        return r.returncode, r.stdout
    monkeypatch.setattr(R, "remote_sh", remote_sh)
    return local_ops, local_repo, far


def test_a_remote_run_records_its_group_and_tees_its_streams_on_the_host(remote_host):
    """User, 2026-09-24 23:35: worker agents on a second computer. The wrapper runs the worker in its own session,
    records the group, and keeps a complete copy of out/run.log on the host - a dropped connection loses nothing."""
    local_ops, local_repo, far = remote_host
    d = f"{local_ops}/work/T-9"
    w = R.to_remote("node2", R.remote_wrapper(f"{local_repo}", "task-t9", "echo RESULT; echo progress >&2; exit 3",
                                              {"HK_WORKER": "1", "A": "x y"}, d, "out.json"))
    r = subprocess.run(["bash", "-c", w], capture_output=True, text=True, timeout=30)
    fd = far / "ops" / "work" / "T-9"
    assert r.returncode == 3 and "RESULT" in r.stdout and "progress" in r.stderr     # streamed to this Mac
    assert (fd / "out.json").read_text().strip() == "RESULT" and "progress" in (fd / "run.log").read_text()
    assert not (fd / "remote.pgid").exists()          # removed when the run ends: a later stop never hits a recycled group


def test_a_remote_run_is_asked_about_and_stopped_explicitly_on_the_host(remote_host):
    """Review FAIL 2026-09-24: a hang-up is not a stop. The group recorded on the host is asked about and stopped."""
    local_ops, local_repo, far = remote_host
    c = {"ticket": "T-9", "host": "node2", "branch": "task-t9", "wt": str(local_repo)}
    d = f"{local_ops}/work/T-9"
    w = R.to_remote("node2", R.remote_wrapper(str(local_repo), "task-t9", "exec sleep 60", {}, d, "out.json"))
    p = subprocess.Popen(["bash", "-c", w], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    for _ in range(50):
        if (far / "ops" / "work" / "T-9" / "remote.pgid").exists():
            break
        time.sleep(0.1)
    time.sleep(0.3)
    assert R.remote_run_state(c) == "running"
    assert R.remote_stop(c, wait_s=5)
    assert R.remote_run_state(c) == "gone"
    p.wait(timeout=10)


def test_the_host_unreachable_or_a_failed_sync_holds_the_claim(remote_host, monkeypatch):
    local_ops, local_repo, far = remote_host
    monkeypatch.setattr(R, "remote_sh", lambda host, cmd, timeout=120, input=None: (255, "ssh: connect timed out"))
    c = {"ticket": "T-9", "host": "node2", "branch": "task-t9", "wt": str(local_repo)}
    assert R.remote_run_state(c) == "unknown" and not R.remote_stop(c, wait_s=1)
    seen = []
    monkeypatch.setattr(R, "attention", lambda *a: seen.append(a))
    monkeypatch.setattr(R.subprocess, "run", lambda args, **kw: subprocess.CompletedProcess(args, 1, b"", b"no route"))
    (local_ops / "work" / "T-9").mkdir(parents=True, exist_ok=True)
    assert R.sync_back(c) is False and [a[2] for a in seen] == ["REMOTE_SYNC"]
    assert R.sync_back(c) is False and len(seen) == 1                                 # warned once, retried each tick


def test_paths_cross_ssh_as_the_hosts_own(remote_host):
    local_ops, local_repo, far = remote_host
    argv = R.ssh_argv("node2", f"cat {local_ops}/work/T-9/handback.json; ls {local_repo}/.claude/worktrees/t9")
    assert argv[0] == "ssh" and "BatchMode=yes" in argv and argv[-2] == "u@h"
    assert f"{far}/ops/work/T-9/handback.json" in argv[-1] and f"{far}/repo/.claude/worktrees/t9" in argv[-1]
    assert str(local_ops) not in argv[-1].replace(str(far), "")


def test_only_a_ticket_named_by_hand_goes_remote_in_stage_one(tmp_path, monkeypatch):
    monkeypatch.setattr(R, "HOSTS_FILE", str(tmp_path / "hosts.json"))
    monkeypatch.setenv("WORK_REMOTE_TICKETS", "T-9, T-11")
    assert R.host_for({"id": "T-9"}) is None                                          # no host configured
    (tmp_path / "hosts.json").write_text(json.dumps({"node2": {"ssh": "u@h", "repo": "/r", "ops": "/o"}}))
    assert R.host_for({"id": "T-9"}) == "node2" and R.host_for({"id": "T-11"}) == "node2"
    assert R.host_for({"id": "T-10"}) is None


def test_a_remote_worker_is_shown_like_a_local_one(tmp_path, monkeypatch):
    """User via supervisor, 2026-09-25 00:40: T-567 ran on node2 and the dashboard showed nothing. Each tick: one
    probe per host into hosts/<host>.json, and each running remote claim's transcript appended into this Mac's
    transcript dir for its worktree (what the dashboard's worker row and transcript modal read)."""
    monkeypatch.setattr(R, "S", str(tmp_path))
    monkeypatch.setattr(R, "HOSTS_FILE", str(tmp_path / "hosts.json"))
    monkeypatch.setattr(R, "PROJECTS", str(tmp_path / "projects"))
    (tmp_path / "hosts.json").write_text(json.dumps({"node2": {"ssh": "u@h", "repo": "/far/repo", "ops": "/far/ops"}}))
    monkeypatch.setattr(R, "remote_sh", lambda h, cmd, timeout=120, input=None: (0, "0.64 0.5 0.6 1/2 3\n24\n534G\n"))
    runs = []
    monkeypatch.setattr(R.subprocess, "run", lambda args, **kw: runs.append(args) or subprocess.CompletedProcess(args, 0, b"", b""))
    claims = {"T-567": {"host": "node2", "state": "running", "session_id": "abc", "wt": f"{R.REPO}/.claude/worktrees/t567"},
              "T-1": {"state": "running", "session_id": "x", "wt": f"{R.REPO}/.claude/worktrees/t1"}}
    R.sync_remote_view(claims, dry=False)
    rec = json.load(open(tmp_path / "hosts" / "node2.json"))
    assert rec["reachable"] and rec["load1"] == 0.64 and rec["cores"] == 24 and rec["disk_free_gb"] == 534
    [rsync] = runs                                                       # only the remote claim
    assert rsync[:3] == ["rsync", "-a", "--append"]
    assert rsync[-2] == "u@h:~/.claude/projects/-far-repo--claude-worktrees-t567/abc.jsonl"
    assert rsync[-1] == f"{tmp_path}/projects/{R._proj_dir(R.REPO)}--claude-worktrees-t567/abc.jsonl"
    monkeypatch.setattr(R, "remote_sh", lambda h, cmd, timeout=120, input=None: (255, "timed out"))
    runs.clear()
    R.sync_remote_view(claims, dry=False)
    assert not json.load(open(tmp_path / "hosts" / "node2.json"))["reachable"] and runs == []


def test_a_reachable_host_below_its_cap_takes_eligible_work_and_the_macs_cap_counts_only_the_mac(tmp_path, monkeypatch):
    """Remote workers stage 2 (supervisor 2026-09-25 00:31: keep node2 going): the remote pass dispatches an eligible
    ticket to a reachable host below its own cap; never the Mac-first GPU paths; the Mac's cap counts only its own."""
    monkeypatch.setattr(R, "S", str(tmp_path))
    monkeypatch.setattr(R, "HOSTS_FILE", str(tmp_path / "hosts.json"))
    monkeypatch.delenv("WORK_REMOTE_TICKETS", raising=False)
    (tmp_path / "hosts.json").write_text(json.dumps({"node2": {"ssh": "u@h", "repo": "/r", "ops": "/o", "cap": 2}}))
    (tmp_path / "hosts").mkdir()
    (tmp_path / "hosts" / "node2.json").write_text(json.dumps({"at": R.time.time(), "reachable": True}))
    remote_claims = {"T-1": {"state": "running", "kind": "work", "host": "node2"}}
    local_claims = {"T-2": {"state": "running", "kind": "work"}, "T-3": {"state": "running", "kind": "fix"}}
    claims = {**remote_claims, **local_claims}
    assert R.busy_workers(claims) == 2 and R.busy_workers(claims, "node2") == 1
    t = {"id": "T-9", "title": "hk-store retention follow-ups", "needs": "none"}
    assert R.host_for(t, claims) == "node2"
    assert R.host_for({"id": "T-10", "title": "wgpu provider for the FFT", "needs": "none"}, claims) is None
    assert R.host_for(t, dict(claims, **{"T-4": {"state": "running", "kind": "work", "host": "node2"}})) is None   # at cap
    (tmp_path / "hosts" / "node2.json").write_text(json.dumps({"at": R.time.time() - 600, "reachable": True}))
    assert R.host_for(t, claims) is None                                          # stale probe: not ready
    (tmp_path / "hosts" / "node2.json").write_text(json.dumps({"at": R.time.time(), "reachable": False}))
    assert R.host_for(t, claims) is None
    monkeypatch.setenv("WORK_REMOTE_TICKETS", "T-10")
    assert R.host_for({"id": "T-10", "title": "wgpu"}, claims) == "node2"         # named by hand: always


def test_the_remote_pass_launches_one_per_host_per_tick_and_its_claims_are_saved(tmp_path, monkeypatch):
    monkeypatch.setattr(R, "S", str(tmp_path))
    monkeypatch.setattr(R, "HOSTS_FILE", str(tmp_path / "hosts.json"))
    monkeypatch.delenv("WORK_REMOTE_TICKETS", raising=False)
    (tmp_path / "hosts.json").write_text(json.dumps({"node2": {"ssh": "u@h", "cap": 3}}))
    (tmp_path / "hosts").mkdir()
    (tmp_path / "hosts" / "node2.json").write_text(json.dumps({"at": R.time.time(), "reachable": True}))
    tasks = [{"id": f"T-{i}", "title": "x", "needs": "none", "status": "todo"} for i in (21, 22)]
    monkeypatch.setattr(R, "board", lambda: tasks)
    monkeypatch.setattr(R, "candidates", lambda ts, cl: [t for t in ts if t["id"] not in cl])
    launched = []
    monkeypatch.setattr(R, "launch", lambda t, dry, host=None: launched.append((t["id"], host)) or {"ticket": t["id"], "host": host, "kind": "work"})
    claims = {}
    assert R.dispatch_remote(claims, dry=False) is True
    assert launched == [("T-21", "node2")] and claims["T-21"]["state"] == "running"


def test_a_new_task_branch_starts_from_the_gated_base_while_a_batch_gates(tmp_path, monkeypatch):
    """2026-09-25 00:31: T-567 was cut from main while the T-577/T-915 batch gated, carrying provisional merges."""
    monkeypatch.setattr(R, "BULKMARK", str(tmp_path / "bulk-in-progress"))
    (tmp_path / "bulk-in-progress").write_text("base=gatedbase123\nbranches=task-t1\n")
    calls = []

    def sh(args, cwd=R.REPO, timeout=120, check=False):
        calls.append(args)
        if args[:3] == ["git", "worktree", "add"]:
            raise RuntimeError("stop here")                  # only the branch point is under test
        return ""
    monkeypatch.setattr(R, "sh", sh)
    with pytest.raises(RuntimeError):
        R.launch({"id": "T-9", "model": "opus"}, dry=False)
    assert calls[-1][:3] == ["git", "worktree", "add"] and calls[-1][-1] == "gatedbase123"


def test_a_named_remote_ticket_never_falls_back_to_the_mac_and_alone_mode_holds_remote_dispatch(tmp_path, monkeypatch):
    """Review 2026-09-25: a named ticket skipped by the remote pass (one launch per host per tick, or a failed prepare)
    was launched on the Mac; and in alone mode remote dispatch kept adding workers the gate was waiting out."""
    monkeypatch.setattr(R, "S", str(tmp_path))
    monkeypatch.setattr(R, "HOSTS_FILE", str(tmp_path / "hosts.json"))
    (tmp_path / "hosts.json").write_text(json.dumps({"node2": {"ssh": "u@h", "cap": 3}}))
    (tmp_path / "hosts").mkdir()
    (tmp_path / "hosts" / "node2.json").write_text(json.dumps({"at": R.time.time(), "reachable": True}))
    monkeypatch.setenv("WORK_REMOTE_TICKETS", "T-31,T-32")
    tasks = [{"id": t, "title": "x", "needs": "none", "status": "todo"} for t in ("T-31", "T-32")]
    monkeypatch.setattr(R, "board", lambda: tasks)
    monkeypatch.setattr(R, "candidates", lambda ts, cl: [t for t in ts if t["id"] not in cl])
    for k, v in {"dispatch_cap": lambda: 3, "disk_free_gb": lambda: 500.0, "gate_holds_dispatch": lambda: False,
                 "queue_depth": lambda: 0}.items():
        monkeypatch.setattr(R, k, v)
    monkeypatch.setattr(R.os, "getloadavg", lambda: (1.0, 1.0, 1.0))
    launched = []
    monkeypatch.setattr(R, "launch", lambda t, dry, host=None: launched.append((t["id"], host)) or {"ticket": t["id"], "host": host, "kind": "work"})
    claims = {}
    R.dispatch(claims, dry=False)
    assert launched == [("T-31", "node2")]                    # T-32 waits for node2's next tick, not the Mac
    monkeypatch.setattr(R, "gate_holds_dispatch", lambda: True)
    launched.clear()
    assert R.dispatch_remote({}, dry=False) is False and launched == []


def test_the_status_file_carries_each_host_and_a_committed_orphan_branch_is_named_not_dispatchable(tmp_path, monkeypatch):
    """Supervisor 2026-09-25 03:56: the status said 'dispatchable=1 (T-844)' while candidates() skips a branch with commits,
    and had no host dimension. Now: T-844-like tickets are 'held_by_branch' + one ORPHAN_BRANCH attention; hosts listed."""
    monkeypatch.setattr(R, "S", str(tmp_path))
    monkeypatch.setattr(R, "HOSTS_FILE", str(tmp_path / "hosts.json"))
    (tmp_path / "hosts.json").write_text(json.dumps({"node2": {"ssh": "u@h", "cap": 5}}))
    (tmp_path / "hosts").mkdir()
    (tmp_path / "hosts" / "node2.json").write_text(json.dumps({"at": R.time.time(), "reachable": True}))
    monkeypatch.setattr(R, "_ORPHANS_SAID", set())
    tasks = [{"id": "T-44", "status": "todo", "title": "x"}, {"id": "T-45", "status": "todo", "title": "y"}]
    monkeypatch.setattr(R, "board", lambda: tasks)
    monkeypatch.setattr(R, "sh", lambda args, cwd=R.REPO, timeout=120, check=False:
                        ("3\n" if "task-t44" in args[-1] else "0\n") if args[:3] == ["git", "rev-list", "--count"] else "abc tip\n")
    seen = []
    monkeypatch.setattr(R, "attention", lambda *a: seen.append(a))
    for k, v in {"load_claims": lambda: {"T-9": {"ticket": "T-9", "state": "running", "kind": "work", "host": "node2"}},
                 "save_claims": lambda c: None, "reap": lambda c, d: False, "dispatch": lambda c, d: False,
                 "release_stale_claims": lambda c, b: False, "sync_board": lambda c, d: None, "reap_worktrees": lambda c, d: None,
                 "reclaim_e2e_data": lambda d: None, "reclaim_idle_targets": lambda c, d: None, "sweep_superseded": lambda d: None,
                 "sync_remote_view": lambda c, d: None, "dispatch_deflakes": lambda c, d: False, "record_queue_depth": lambda: None,
                 "gate_holds_dispatch": lambda: False, "dispatch_cap": lambda: 2, "disk_free_gb": lambda: 400.0}.items():
        monkeypatch.setattr(R, k, v)
    R.tick(dry=False)
    st = json.load(open(tmp_path / "work-runner-status.json"))
    assert st["frontier"]["held_by_branch"] == ["T-44"] and st["frontier"]["dispatchable_ids"] == ["T-45"]
    assert st["hosts"]["node2"] == {"running": 1, "cap": 5, "ready": True, "probe_age_s": 0}
    assert st["hosts"]["mac"] == {"running": 0, "cap": 2}
    assert [a[2] for a in seen] == ["ORPHAN_BRANCH"]
    R.tick(dry=False)
    assert len(seen) == 1                                                  # once per runner process
