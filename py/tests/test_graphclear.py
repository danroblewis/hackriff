"""When the open ticket graph clears (py/hkpy/graphclear.py; user, 2026-09-24)."""
from __future__ import annotations

from datetime import datetime, timedelta

from hkpy import graphclear as G

NOW = datetime(2026, 9, 24, 2, 0)
SAMPLES = [60.0, 120.0, 180.0, 240.0, 300.0]          # dispatch -> landed minutes; p50 180


def T(i, status="todo", deps=(), **kw):
    return dict({"id": i, "status": status, "depends_on": list(deps)}, **kw)


def test_scope_excludes_what_needs_someone_outside_and_says_why():
    tasks = [T("T-1"), T("T-2", "in-progress"), T("T-3", "deferred"), T("T-4", "blocked", blocked_on="HIL"),
             T("T-5", blocked_on="user capture"), T("T-6", needs="hardware"), T("T-7", deps=["T-3"]),
             T("T-8", deps=["T-7"]), T("T-9", deps=["T-9"]), T("T-10", "done"), T("T-11", "cancelled")]
    r = G.estimate(tasks, NOW, SAMPLES, {}, {}, 2.0, 2.0)
    assert r["n"] == 2
    assert r["excluded"] == {"blocked (needs user/hardware)": ["T-4", "T-5", "T-6"], "deferred": ["T-3"],
                             "dependency cycle (or waits on one)": ["T-9"],
                             "waits on a deferred/blocked ticket": ["T-7", "T-8"]}


def test_a_chain_of_dependents_adds_its_durations_and_names_the_critical_path():
    tasks = [T("T-1"), T("T-2", deps=["T-1"]), T("T-3", deps=["T-2"]), T("T-4")]
    r = G.estimate(tasks, NOW, SAMPLES, {}, {}, 60.0, 60.0)          # throughput never binds
    p50 = r["eta"]["p50"]
    assert r["bound"] == "chain" and r["chain"] == ["T-1", "T-2", "T-3"]
    assert datetime.fromtimestamp(p50["at"]) == NOW + timedelta(minutes=3 * 180)
    assert r["eta"]["p25"]["at"] < p50["at"] < r["eta"]["p75"]["at"]
    line = G.line(r, NOW)
    assert line.startswith("open graph clears ~11:00 (4 tickets; p25-p75 ~") and "set by the chain T-1 -> T-2 -> T-3" in line


def test_a_wide_frontier_is_bound_by_throughput():
    tasks = [T(f"T-{i}") for i in range(20)]
    r = G.estimate(tasks, NOW, SAMPLES, {}, {}, 4.0, 2.0)
    assert r["bound"] == "throughput"
    assert datetime.fromtimestamp(r["eta"]["p50"]["at"]) == NOW + timedelta(hours=10)     # 20 / 2.0 per h (24 h rate)
    assert r["eta"]["p25"]["throughput_at"] < r["eta"]["p75"]["throughput_at"]            # faster / slower rate
    assert "set by throughput 2.0/h" in G.line(r, NOW)


def test_an_overdue_ticket_is_not_landing_now_and_a_queued_one_uses_the_queue():
    tasks = [T("T-1", "in-progress"), T("T-2", deps=["T-1"]), T("T-3", "in-progress")]
    r = G.estimate(tasks, NOW, SAMPLES, {"T-1": (NOW - timedelta(minutes=200)).timestamp()},
                   {"T-3": NOW + timedelta(minutes=40)}, 60.0, 60.0)
    # 200 min in: the samples longer than that are 240 and 300 -> 40..100 min left, not "now"
    ef1 = r["eta"]["p50"]["chain_at"] - 180 * 60
    assert NOW + timedelta(minutes=40) <= datetime.fromtimestamp(ef1) <= NOW + timedelta(minutes=100)
    only_queued = G.estimate([T("T-3", "in-progress")], NOW, SAMPLES, {}, {"T-3": NOW + timedelta(minutes=40)}, 60.0, 60.0)
    assert datetime.fromtimestamp(only_queued["eta"]["p50"]["chain_at"]) == NOW + timedelta(minutes=40)


def test_nothing_in_scope_and_no_samples_say_so():
    assert G.line(G.estimate([T("T-1", "deferred")], NOW, SAMPLES, {}, {}, 1, 1), NOW) == \
        "open graph: nothing in scope (excluded 1: deferred 1)"
    assert "no estimate (no dispatch->landed samples)" in G.line(G.estimate([T("T-1")], NOW, [], {}, {}, 1, 1), NOW)


def test_samples_are_dispatch_to_landing_over_seven_days():
    log = "\n".join(["[09-23 10:00:00] DISPATCH T-5 [sonnet/medium] pid=1", "[09-23 12:00:00] DISPATCH T-5 [sonnet/medium] pid=2",
                     "[09-10 10:00:00] DISPATCH T-6 [sonnet/medium] pid=3"])
    disp = G.dispatch_times(log, 2026)
    landed = [{"ticket": "T-5", "branch": "task-t5", "merge_ts": datetime(2026, 9, 23, 13, 0).timestamp()},
              {"ticket": "task-t6-rl", "branch": "task-t6-rl", "merge_ts": datetime(2026, 9, 10, 11, 0).timestamp()}]
    assert G.samples_from(disp, landed, NOW) == [60.0]          # the latest dispatch; T-6 is older than 7 days


def test_the_ledger_resolves_each_prediction_against_the_first_tick_that_saw_it_happen(tmp_path):
    rows = [{"ts": 0, "kind": "open_graph", "predicted": 3600, "state": 5},
            {"ts": 600, "kind": "open_graph", "predicted": 4000, "state": 3},
            {"ts": 4200, "kind": "open_graph", "predicted": None, "state": 0},
            {"ts": 5000, "kind": "open_graph", "predicted": 9000, "state": 2},
            {"ts": 0, "kind": "queue_clears", "predicted": 100, "state": 1}]
    assert G.accuracy(rows, "open_graph") == {"resolved": 2, "pending": 1, "median_error_min": 6.7}   # +10 and +3.3 min
    assert G.accuracy(rows, "queue_clears") == {"resolved": 0, "pending": 1, "median_error_min": None}
    r = G.estimate([T("T-1")], NOW, SAMPLES, {}, {}, 2.0, 2.0)
    G.record(str(tmp_path), NOW, r, NOW + timedelta(minutes=30), 2, True)
    G.record(str(tmp_path), NOW + timedelta(hours=1), G.estimate([], NOW, SAMPLES, {}, {}, 2, 2), None, 0, False)
    assert G.accuracy_line(str(tmp_path)) == ("ETA ledger - queue-clears: median error +30.0 min over 1, 0 pending; "
                                              "open graph: median error -120.0 min over 1, 0 pending")
