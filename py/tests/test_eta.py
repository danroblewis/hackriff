"""hkpy.eta - the digest's "queue clears ~HH:MM; T-801 lands ~HH:MM" (user, 2026-09-24)."""

from datetime import datetime, timedelta

from hkpy import eta

NOW = datetime(2026, 9, 24, 1, 0)


def test_the_queue_clears_after_the_running_gate_and_one_gate_per_batch():
    started = NOW - timedelta(minutes=10)                    # 24-min gates: 14 left
    assert eta.queue_clears(NOW, 0, started, 24) == NOW + timedelta(minutes=14)
    assert eta.queue_clears(NOW, 3, started, 24) == NOW + timedelta(minutes=14 + 24)
    assert eta.queue_clears(NOW, 16, started, 24, bulk_max=15) == NOW + timedelta(minutes=14 + 48)
    assert eta.queue_clears(NOW, 0, None, 24) is None
    assert eta.queue_clears(NOW, 2, NOW - timedelta(hours=2), 24) == NOW + timedelta(minutes=24)   # overdue gate: 0 left


def test_a_ticket_lands_by_where_it_is():
    q = ["task-t1", "task-t801"]
    t, why = eta.ticket_lands(NOW, "T-801", "task-t801", q, None, None, 24, 21, 2)
    assert t == NOW + timedelta(minutes=24) and "position 2" in why
    claim = {"state": "running", "kind": "work", "started": (NOW - timedelta(minutes=5)).timestamp()}
    t, why = eta.ticket_lands(NOW, "T-801", "task-t801", [], claim, None, 24, 21, 2)
    assert t == NOW + timedelta(minutes=16 + 2 + 24) and why == "a worker is on it"
    for state in ("review-failed", "blocked", "gate-failed"):
        t, why = eta.ticket_lands(NOW, "T-801", "task-t801", [], {"state": state}, None, 24, 21, 2)
        assert t is None and state in why
    assert eta.ticket_lands(NOW, "T-9", "task-t9", [], None, None, 24, 21, 2)[0] is None


def test_the_line_says_what_it_is():
    line = eta.digest_line(NOW, NOW + timedelta(minutes=40), 3, "T-801", None, "unblocks 19; review-failed - needs the coordinator")
    assert line == ("ETA: queue clears ~01:40 (3 queued); T-801: no estimate - unblocks 19; review-failed - "
                    "needs the coordinator - medians, flakes and reds not modelled")
    assert eta.digest_line(NOW, None, 0, None, None, "") == "ETA: queue empty, nothing gating"


def test_the_digest_carries_the_eta_line(tmp_path, monkeypatch):
    from hkpy import flow
    monkeypatch.setattr(flow, "eta_line", lambda ops, now, repo=None: "ETA: queue clears ~01:40 (3 queued)")
    s = {"ts": NOW.timestamp(), "at": "x", "landings_per_h_6h": 1.0, "landings_per_h_24h": 1.0, "reds_24h": 0,
         "gates_24h": 1, "real_reds_24h": 0, "flakes_24h": 0, "touchpoints_24h": 0}
    got = {}
    flow.digest(str(tmp_path), s, NOW, lambda level, title, body, key: got.update({key: body}))
    assert "\nETA: queue clears ~01:40 (3 queued)" in got["flow:digest"]
