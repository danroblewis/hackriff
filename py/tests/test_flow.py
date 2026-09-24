"""`just flow` reads the ops logs and says where the hours went (pipeline manager, 2026-09-23)."""

import json
from datetime import datetime

from hkpy import flow

LOG = """\
[09-23 05:13:37] === merge-runner up (DRY_RUN=0, bulk mode); watching q ===
[09-23 05:13:40] WAIT: 1 worker(s) running - the gate runs alone, dispatch is paused
[09-23 05:13:40] WAIT: 1 worker(s) running - the gate runs alone, dispatch is paused
[09-23 05:20:00] QUEUED task-t608
[09-23 05:58:46] WAIT over: 1 worker(s) running still, after 2700 s - gating anyway
[09-23 05:58:46] BULK attempt (3): task-t607 task-t608 task-t464
[09-23 05:58:49] BULK conflict merging task-t464 -> SKIPPED, batch continues with the rest
[09-23 05:58:50] BULK gate (just gate --base abc over 2 merged branches; may take 15-25 min)…
gate: just lint took 23s (exit 0)
gate: just test took 1797s (exit 0)
gate: just acceptance-ci took 568s (exit 0)
gate: just test-ui-e2e took 218s (exit 1)
[09-23 06:40:00] TRIAGE: browser specs red: x.e2e.mjs - re-running them alone
[09-23 06:40:30] TRIAGE: they PASS alone -> load flake; retrying the gate's acceptance phase once
gate: just test-ui-e2e took 219s (exit 0)
[09-23 06:42:24] TRIAGE: retry PASSED
[09-23 06:42:24] BULK MERGED ✓ T-607 T-608
[09-23 06:42:24] BULK MERGED ✓ T-607 T-608
[09-23 07:00:00] MERGE start task-t846 (T-846, 3 commits ahead)
[09-23 07:00:01] GATE task-t846 (just gate-merge; may take 15-25 min)…
gate: just test-ui took 7s (exit 0)
[09-23 07:05:00] GATE FAILED task-t846 (attempt 1/2, tip deadbeef) -> abort + flag for AI
"""

WORK = """\
[09-23 05:00:00] DISPATCH T-607 [opus/high] pid=1 -> /wt/t607
[09-23 05:01:00] DISPATCH T-608 [opus/high] pid=2 -> /wt/t608
[09-23 06:30:00] VERSION: matches HEAD:ops/work-runner.py  ops=/x cap=6 dry=False
"""


def _ops(tmp_path):
    (tmp_path / "merge-runner.log").write_text(LOG, encoding="utf-8")
    (tmp_path / "work-runner.log").write_text(WORK, encoding="utf-8")
    t = lambda s: datetime(2026, 9, 23, *map(int, s.split(":"))).timestamp()  # noqa: E731
    (tmp_path / "handbacks.jsonl").write_text(
        json.dumps({"ts": t("05:50"), "ticket": "T-607", "outcome": "done"}) + "\n", encoding="utf-8")
    (tmp_path / "landed.jsonl").write_text(
        json.dumps({"ticket": "T-607", "branch": "task-t607", "merge_ts": t("06:42"), "gate_attempts": 1}) + "\n"
        + json.dumps({"ticket": "T-608", "branch": "task-t608", "merge_ts": t("06:42"), "gate_attempts": 1}) + "\n"
        + json.dumps({"ticket": "task-ops", "branch": "task-ops", "merge_ts": t("06:42"), "gate_attempts": 1}) + "\n",
        encoding="utf-8")
    (tmp_path / "merge-queue.txt").write_text("task-a\n# c\n\ntask-b\n", encoding="utf-8")
    (tmp_path / "merge-needs-attention.txt").write_text(
        "09-23 05:58  task-t464  T-464  CONFLICT(skipped from bulk)\n[09-23 06:00] supervisor: a note\n", encoding="utf-8")
    return str(tmp_path)


def test_duplicate_log_lines_collapse_to_one_event():
    evs = flow.parse_log(LOG, 2026)
    waits = [e for e in evs if e.text.startswith("WAIT: ")]
    assert len(waits) == 1
    merged = [e for e in evs if e.text.startswith("BULK MERGED")]
    assert len(merged) == 1


def test_gates_carry_suites_wait_verdict_cause_and_batch():
    gates = flow.gates_from(flow.parse_log(LOG, 2026))
    assert len(gates) == 2
    g = gates[0]
    assert g.klass == "full" and g.ok is True and g.cause == "flake"
    assert round(g.wait_min) == 45          # 05:13:40 -> 05:58:50
    assert len(g.branches) == 3 and g.conflicts == ["task-t464"]
    assert [c for c, _, _ in g.suites][:2] == ["just lint", "just test"]
    assert round(g.minutes) == 44           # 05:58:50 -> 06:42:24
    g2 = gates[1]
    assert g2.klass == "ui" and g2.ok is False and g2.cause == "real" and g2.branches == ["task-t846"]


def test_hourly_table_attributes_dispatch_handback_landed_gate_wait_red_and_conflicts(tmp_path):
    ops = _ops(tmp_path)
    rows = {r["hour"]: r for r in flow.hourly(ops, datetime(2026, 9, 23, 5), datetime(2026, 9, 23, 8))}
    assert rows["09-23 05"]["dispatch"] == 2 and rows["09-23 06"]["dispatch"] == 0
    assert rows["09-23 05"]["handback"] == 1
    assert rows["09-23 06"]["landed"] == 2            # T-607, T-608; the ops branch is not a ticket
    assert rows["09-23 05"]["conflicts"] == 1
    assert rows["09-23 05"]["wait_min"] == 45 and rows["09-23 05"]["gate_min"] == 1
    assert rows["09-23 06"]["gate_min"] == 42
    assert rows["09-23 07"]["red"] == 1 and rows["09-23 06"]["red"] == 0


def test_gate_rows_and_ticket_rows(tmp_path):
    ops = _ops(tmp_path)
    since, until = datetime(2026, 9, 23, 5), datetime(2026, 9, 23, 8)
    g = flow.gate_rows(ops, since, until)
    assert g[0]["batch"] == 2 and g[0]["conflicts"] == 1 and g[0]["verdict"] == "green" and g[0]["cause"] == "flake"
    assert g[1]["verdict"] == "red" and g[1]["cause"] == "real"
    t = {r["ticket"]: r for r in flow.ticket_rows(ops, since, until)}
    assert t["T-607"]["work_min"] == 50 and t["T-607"]["gate_min"] == 43 and t["T-607"]["total_min"] == 102
    assert t["T-608"]["queue_wait_min"] == 39          # QUEUED 05:20 -> gate 05:58:50 (no handback record)


def test_summary_line_names_the_numbers_a_person_reads(tmp_path):
    ops = _ops(tmp_path)
    s = flow.summary(ops, now=datetime(2026, 9, 23, 8))
    assert s["gates_24h"] == 2 and s["reds_24h"] == 1 and s["real_reds_24h"] == 1 and s["flakes_24h"] == 1
    assert s["queue_depth"] == 2 and s["worker_cap"] == 6 and s["conflicts_24h"] == 1
    assert s["touchpoints_24h"] == 1                     # the CONFLICT line, not the supervisor's note
    assert s["hours_with_dispatch_24h"] == 1
    line = flow.summary_line(s)
    assert line.startswith("flow: ") and "reds 1/2 (1 real) · flakes 1" in line and "queue 2" in line


def test_touchpoints_count_holds(tmp_path):
    ops = _ops(tmp_path)
    (tmp_path / "hold.jsonl").write_text(
        json.dumps({"ts": datetime(2026, 9, 23, 6, 50).timestamp(), "event": "hold", "minutes": 20, "why": "incident"}) + "\n",
        encoding="utf-8")
    tp = flow.touchpoints(ops, datetime(2026, 9, 23, 5), datetime(2026, 9, 23, 8))
    assert len(tp) == 2 and any("hold 20 min" in x for x in tp)


def test_record_appends_one_json_line(tmp_path, capsys):
    ops = _ops(tmp_path)
    assert flow.main(["--ops", ops, "--since", "2026-09-23T05:00", "--record"]) == 0
    lines = (tmp_path / "flow.jsonl").read_text().splitlines()
    assert len(lines) == 1 and json.loads(lines[0])["gates_24h"] >= 0
    assert "== hourly" in capsys.readouterr().out


# --------------------------------------------------------------------- digest (2026-09-23)
def _s(ts, **kw):
    base = {"ts": ts, "at": datetime.fromtimestamp(ts).strftime("%Y-%m-%d %H:%M"), "landings_per_h_6h": 1.2,
            "landings_per_h_24h": 0.9, "reds_24h": 3, "gates_24h": 10, "real_reds_24h": 1, "flakes_24h": 1,
            "touchpoints_24h": 2}
    base.update(kw)
    return base


def test_the_tick_line_is_invariant_23s_shape(tmp_path):
    now = datetime(2026, 9, 23, 16)
    (tmp_path / "hold").write_text(f"until={int(now.timestamp()) + 600}\nsince=0\nowner=pm\nwhy=incident: x\n")
    line = flow.tick_line(str(tmp_path), _s(now.timestamp()), now)
    assert line == ("flow: 1.2/h (6h) 0.9/h (24h) · reds 3/10 (1 real) · flakes 1 · touchpoints 2 · "
                    "no experiment · holding: until 16:10 incident: x")
    (tmp_path / "hold").write_text(f"until={int(now.timestamp()) - 1}\n")          # expired = none
    assert line.replace("until 16:10 incident: x", "none") == flow.tick_line(str(tmp_path), _s(now.timestamp()), now)


def test_trend_breaks_need_a_real_move_against_two_hours_ago():
    t = 1_000_000.0
    hist = [_s(t - 3 * 3600, landings_per_h_6h=2.0, real_reds_24h=1), _s(t - 1800, touchpoints_24h=2)]
    assert flow.trend_breaks(_s(t, landings_per_h_6h=1.1), hist, False) == []       # 2.0 -> 1.1 is not half
    kinds = [k for k, _ in flow.trend_breaks(_s(t, landings_per_h_6h=1.0, real_reds_24h=3, touchpoints_24h=3), hist, True)]
    assert kinds == ["landings", "reds", "touchpoint", "hold"]
    tiny = [_s(t - 3 * 3600, landings_per_h_6h=0.4, real_reds_24h=0)]
    assert flow.trend_breaks(_s(t, landings_per_h_6h=0.0, real_reds_24h=1), tiny, False) == []   # small numbers


def test_digest_posts_every_two_hours_and_each_break_at_once(tmp_path):
    ops = str(tmp_path)
    now = datetime(2026, 9, 23, 16)
    sent = []
    send = lambda level, title, body, key: sent.append((level, key))              # noqa: E731
    assert flow.digest(ops, _s(now.timestamp()), now, send) == ["flow:digest"]
    assert sent == [("green", "flow:digest")]
    (tmp_path / "alerts.jsonl").write_text(json.dumps({"ts": now.timestamp() - 3600, "key": "flow:digest", "status": "sent"}) + "\n")
    sent.clear()
    assert flow.digest(ops, _s(now.timestamp()), now, send) == []                  # 1 h since the last
    (tmp_path / "flow.jsonl").write_text(json.dumps(_s(now.timestamp() - 1800, touchpoints_24h=1)) + "\n")
    assert flow.digest(ops, _s(now.timestamp()), now, send) == ["flow:break:touchpoint"]
    assert sent == [("amber", "flow:break:touchpoint")]



def test_accepted_flakes_reach_the_tick_line_and_the_digest(tmp_path):
    ops = str(tmp_path)
    now = datetime(2026, 9, 23, 18)
    (tmp_path / "flaky.jsonl").write_text(
        json.dumps({"ts": "2026-09-23T17:10:00", "tests": "fog-of-war.e2e.mjs", "batch": "T-1 T-2", "accepted": True,
                    "suite": "test-ui-e2e", "kind": "spec", "saved_s": 780}) + "\n"
        + json.dumps({"ts": "2026-09-23T17:20:00", "tests": "x", "batch": "T-3"}) + "\n")   # old-style, not accepted
    s = dict(_s(now.timestamp()), flake_accepts_24h=1, flake_saved_min_24h=13)
    assert flow.tick_line(ops, s, now).endswith(" · flake-accepts 1 (saved 13 min)")
    assert [o["tests"] for o in flow.flake_accepts(ops, datetime(2026, 9, 23), now)] == ["fog-of-war.e2e.mjs"]
    bodies = []
    flow.digest(ops, s, now, lambda level, title, body, key: bodies.append((key, body)))
    digest = dict(bodies)["flow:digest"]
    assert "flake accepted 17:10: fog-of-war.e2e.mjs in `just test-ui-e2e` (T-1 T-2) - passed alone twice, ~13 min saved" in digest
