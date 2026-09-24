"""`just flow` — where the pipeline's hours went, from the ops logs. Read-only.

WHY. On 2026-09-23 the burndown went flat at ~1 ticket/hour and the question "are the work
agents slow, or is the work done but failing tests?" took twenty shell calls to answer. The
answer was neither: dispatch had been zero in 10 of 13 hours because the gate ran alone and held
the box 40-60 minutes of each. That table is this module's `--hourly`; the other views are the
two follow-up questions it always raises (which gate cost what, and where one ticket's time went).

Sources (all under `$HACKRIFF_OPS`, all already written by the runners; nothing here mutates):
  merge-runner.log   gate begin/end, suites, WAIT/OVERLAP, attempts, conflicts, TRIAGE, QUEUED
  work-runner.log    DISPATCH lines
  landed.jsonl       {ticket, merge_ts, land_minutes, gate_attempts}
  handbacks.jsonl    {ts, ticket, outcome}
  work-claims.json   running workers
  merge-queue.txt    depth
  merge-needs-attention.txt, hold.jsonl   what a person had to do (touchpoints)

The merge-runner log writes every line twice (`log()` tees into the same file its stdout is
redirected to), so events are deduplicated on (timestamp, text) exactly as `hkpy.cycletime` does.
The log has no year; it is taken from the file's mtime and stepped back across a December wrap.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from datetime import datetime, timedelta

OPS = os.environ.get("HACKRIFF_OPS") or os.path.expanduser("~/.hackriff-ops")

_LINE = re.compile(r"^\[(\d\d)-(\d\d) (\d\d):(\d\d):(\d\d)\] (.*)$")
_SUITE = re.compile(r"^gate: (just [\w-]+) took (\d+)s \(exit (-?\d+)\)")
_GATE_BEGIN = re.compile(r"^(?:BULK gate \(|GATE (\S+) \(just gate-merge)")
_GATE_END_OK = re.compile(r"^(?:BULK MERGED ✓(.*)|MERGED (\S+) ✓)")
_GATE_END_BAD = re.compile(r"^(?:BULK gate FAILED|GATE FAILED|GATE TIMEOUT|BULK gate TIMED OUT)")
_ATTEMPT = re.compile(r"^BULK attempt \((\d+)\): (.*)$")
_CONFLICT = re.compile(r"^BULK conflict merging (\S+)")
_WAIT = re.compile(r"^WAIT: ")
_WAIT_OVER = re.compile(r"^(?:WAIT over|OVERLAP): ")
_QUEUED = re.compile(r"^QUEUED (\S+)")
_DISPATCH = re.compile(r"DISPATCH (T-\d+[a-z]?) ")
_TRIAGE_FLAKE = re.compile(r"^TRIAGE: (?:they PASS alone|retry PASSED)")
_TRIAGE_REAL = re.compile(r"^TRIAGE: (?:retry FAILED too|MAIN IS RED|.*fails alone)")
_TRIAGE_SUITE = re.compile(r"^BULK gate FAILED without a test FAIL")


@dataclass
class Ev:
    t: datetime
    text: str


@dataclass
class Gate:
    start: datetime
    end: datetime | None = None
    ok: bool | None = None
    suites: list[tuple[str, int, int]] = field(default_factory=list)  # (cmd, seconds, rc)
    attempt: int = 0
    branches: list[str] = field(default_factory=list)
    conflicts: list[str] = field(default_factory=list)
    cause: str = ""          # "" | flake | real | suite-broken | timeout
    wait_min: float = 0.0    # drain/foreign/contention wait BEFORE this gate

    @property
    def klass(self) -> str:
        cmds = {c for c, _, _ in self.suites}
        if "just test" in cmds:
            return "full"
        if cmds & {"just test-ui-e2e", "just test-ui"}:
            return "ui"
        if cmds & {"just test-py", "just lint-py"}:
            return "py"
        return "?" if not cmds else "+".join(sorted(c.split()[1] for c in cmds))

    @property
    def minutes(self) -> float | None:
        return None if self.end is None else (self.end - self.start).total_seconds() / 60


def _read(path: str) -> str:
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            return fh.read()
    except FileNotFoundError:
        return ""


def _jsonl(path: str) -> list[dict]:
    out = []
    for line in _read(path).splitlines():
        try:
            out.append(json.loads(line))
        except ValueError:
            continue
    return out


def parse_log(text: str, year: int) -> list[Ev]:
    """Timestamped lines, deduplicated, year-stepped across a December wrap."""
    out: list[Ev] = []
    seen: set[tuple[datetime, str]] = set()
    prev: datetime | None = None
    cur_year = year
    for raw in text.splitlines():
        m = _LINE.match(raw.strip())
        if not m:
            # The gate's own suite lines (`gate: just test took 1797s (exit 0)`) are written by
            # hkpy.gate straight into the log with NO timestamp; they belong to the gate whose
            # begin line preceded them, so they take the last timestamp seen.
            if prev is not None and _SUITE.match(raw.strip()):
                out.append(Ev(prev, raw.strip()))
            continue
        mon, day, hh, mm, ss, rest = m.groups()
        try:
            when = datetime(cur_year, int(mon), int(day), int(hh), int(mm), int(ss))
        except ValueError:
            continue
        if prev is not None and when < prev - timedelta(days=200):
            cur_year += 1
            when = when.replace(year=cur_year)
        prev = when
        key = (when, rest)
        if key in seen:
            continue
        seen.add(key)
        out.append(Ev(when, rest))
    return out


def _log_year(path: str) -> int:
    try:
        return datetime.fromtimestamp(os.path.getmtime(path)).year
    except OSError:
        return datetime.now().year


def gates_from(events: list[Ev]) -> list[Gate]:
    """Gate intervals with their suites, verdict, cause and the wait that preceded them."""
    gates: list[Gate] = []
    cur: Gate | None = None
    wait_since: datetime | None = None
    pending_attempt: tuple[int, list[str]] | None = None
    pending_conflicts: list[str] = []
    for ev in events:
        s = ev.text
        if _WAIT.match(s) and wait_since is None:
            wait_since = ev.t
            continue
        if _WAIT_OVER.match(s):
            continue
        m = _ATTEMPT.match(s)
        if m:
            pending_attempt = (int(m.group(1)), m.group(2).split())
            pending_conflicts = []
            continue
        m = _CONFLICT.match(s)
        if m:
            pending_conflicts.append(m.group(1))
            continue
        if _GATE_BEGIN.match(s):
            if cur is not None and cur.end is None:   # a gate that never reported (killed runner)
                cur.end, cur.ok, cur.cause = ev.t, False, cur.cause or "killed"
            cur = Gate(start=ev.t)
            if wait_since is not None:
                cur.wait_min = (ev.t - wait_since).total_seconds() / 60
                wait_since = None
            if pending_attempt:
                cur.attempt, cur.branches = pending_attempt
                cur.conflicts = list(pending_conflicts)
                pending_attempt = None
            else:
                mm = _GATE_BEGIN.match(s)
                if mm and mm.group(1):
                    cur.branches = [mm.group(1)]
            gates.append(cur)
            continue
        if cur is None:
            continue
        m = _SUITE.match(s)
        if m:
            cur.suites.append((m.group(1), int(m.group(2)), int(m.group(3))))
            continue
        if _TRIAGE_SUITE.match(s):
            cur.cause = "suite-broken"
        elif _TRIAGE_REAL.match(s):
            cur.cause = "real"
        elif _TRIAGE_FLAKE.match(s):
            cur.cause = cur.cause or "flake"
        if _GATE_END_OK.match(s) and cur.end is None:
            cur.end, cur.ok = ev.t, True
            if cur.cause == "":
                cur.cause = "green"
        elif _GATE_END_BAD.match(s) and cur.end is None:
            cur.end, cur.ok = ev.t, False
            if "TIMEOUT" in s or "TIMED OUT" in s:
                cur.cause = "timeout"
            elif cur.cause in ("", "flake"):
                cur.cause = "real" if cur.cause == "" else "flake-then-real"
    return gates


def _hour(t: datetime) -> str:
    return t.strftime("%m-%d %H")


def _overlap_minutes(a0: datetime, a1: datetime, b0: datetime, b1: datetime) -> float:
    lo, hi = max(a0, b0), min(a1, b1)
    return max(0.0, (hi - lo).total_seconds() / 60)


def hourly(ops: str, since: datetime, until: datetime) -> list[dict]:
    """One row per hour: dispatch, handback, landed, gate_min, wait_min, red, conflicts."""
    mr = os.path.join(ops, "merge-runner.log")
    events = parse_log(_read(mr), _log_year(mr))
    gates = gates_from(events)
    wr = os.path.join(ops, "work-runner.log")
    wevents = parse_log(_read(wr), _log_year(wr))
    rows: dict[str, Counter] = defaultdict(Counter)
    for ev in wevents:
        if since <= ev.t <= until and _DISPATCH.search(ev.text):
            rows[_hour(ev.t)]["dispatch"] += 1
    for hb in _jsonl(os.path.join(ops, "handbacks.jsonl")):
        t = datetime.fromtimestamp(float(hb.get("ts", 0)))
        if since <= t <= until:
            rows[_hour(t)]["handback"] += 1
    for ld in _jsonl(os.path.join(ops, "landed.jsonl")):
        t = datetime.fromtimestamp(float(ld.get("merge_ts", 0)))
        if since <= t <= until and str(ld.get("ticket", "")).startswith("T-"):
            rows[_hour(t)]["landed"] += 1
    for g in gates:
        end = g.end or until
        if end < since or g.start > until:
            continue
        h = g.start.replace(minute=0, second=0, microsecond=0)
        while h < end:
            nxt = h + timedelta(hours=1)
            rows[_hour(h)]["gate_min"] += _overlap_minutes(g.start, end, h, nxt)
            h = nxt
        if g.wait_min:
            w0 = g.start - timedelta(minutes=g.wait_min)
            h = w0.replace(minute=0, second=0, microsecond=0)
            while h < g.start:
                nxt = h + timedelta(hours=1)
                rows[_hour(h)]["wait_min"] += _overlap_minutes(w0, g.start, h, nxt)
                h = nxt
        if g.ok is False and since <= end <= until:
            rows[_hour(end)]["red"] += 1
        if g.conflicts and since <= g.start <= until:
            rows[_hour(g.start)]["conflicts"] += len(g.conflicts)
    out = []
    h = since.replace(minute=0, second=0, microsecond=0)
    while h <= until:
        r = rows.get(_hour(h), Counter())
        out.append({"hour": _hour(h), "dispatch": r["dispatch"], "handback": r["handback"],
                    "landed": r["landed"], "gate_min": round(r["gate_min"]), "wait_min": round(r["wait_min"]),
                    "red": r["red"], "conflicts": r["conflicts"]})
        h += timedelta(hours=1)
    return out


def gate_rows(ops: str, since: datetime, until: datetime) -> list[dict]:
    mr = os.path.join(ops, "merge-runner.log")
    gates = gates_from(parse_log(_read(mr), _log_year(mr)))
    out = []
    for g in gates:
        if g.start < since or g.start > until:
            continue
        out.append({"start": g.start.strftime("%m-%d %H:%M"), "class": g.klass,
                    "minutes": None if g.minutes is None else round(g.minutes),
                    "wait_min": round(g.wait_min), "verdict": "green" if g.ok else ("open" if g.ok is None else "red"),
                    "cause": g.cause or ("open" if g.ok is None else ""), "batch": len(g.branches) - len(g.conflicts),
                    "conflicts": len(g.conflicts),
                    "suites": " ".join(f"{c.split()[1]}={s}s{'' if rc == 0 else '!'}" for c, s, rc in g.suites)})
    return out


def ticket_rows(ops: str, since: datetime, until: datetime) -> list[dict]:
    """Per landed ticket: dispatched -> handback -> queued -> gate start -> landed, with the waits."""
    mr = os.path.join(ops, "merge-runner.log")
    events = parse_log(_read(mr), _log_year(mr))
    gates = gates_from(events)
    wr = os.path.join(ops, "work-runner.log")
    dispatched: dict[str, datetime] = {}
    for ev in parse_log(_read(wr), _log_year(wr)):
        m = _DISPATCH.search(ev.text)
        if m and m.group(1) not in dispatched:
            dispatched[m.group(1)] = ev.t
    handback: dict[str, datetime] = {}
    for hb in _jsonl(os.path.join(ops, "handbacks.jsonl")):
        handback.setdefault(str(hb.get("ticket")), datetime.fromtimestamp(float(hb.get("ts", 0))))
    queued: dict[str, datetime] = {}
    for ev in events:
        m = _QUEUED.match(ev.text)
        if m:
            queued.setdefault(m.group(1), ev.t)
    out = []
    for ld in _jsonl(os.path.join(ops, "landed.jsonl")):
        t_land = datetime.fromtimestamp(float(ld.get("merge_ts", 0)))
        tid, br = str(ld.get("ticket", "")), str(ld.get("branch", ""))
        if not (since <= t_land <= until):
            continue
        t_disp, t_hb, t_q = dispatched.get(tid), handback.get(tid), queued.get(br)
        t_gate = next((g.start for g in gates if br in g.branches or tid in g.branches), None)

        def mins(a: datetime | None, b: datetime | None) -> int | None:
            return None if a is None or b is None else round((b - a).total_seconds() / 60)

        out.append({"ticket": tid, "dispatched": t_disp and t_disp.strftime("%m-%d %H:%M"),
                    "work_min": mins(t_disp, t_hb), "queue_wait_min": mins(t_hb or t_q, t_gate),
                    "gate_min": mins(t_gate, t_land), "total_min": mins(t_disp, t_land),
                    "landed": t_land.strftime("%m-%d %H:%M"), "attempts": ld.get("gate_attempts")})
    return out


#: A merge-runner CONFLICT / GATE_FAIL line is the WORK RUNNER's input, not a person's: it re-queues a
#: branch that merges cleanly again, or resumes the worker for a fix run. Counted as a touchpoint only
#: when nothing took it within this long - a conflict run waits for a free worker slot, and T-613's
#: re-queue came 1 h 40 min after its line. On 2026-09-24 20 of the 29 "touchpoints" in 24 h were
#: such lines, and T-875's (conflict-fixed and re-queued by the runner in 7 min) fired a false
#: Discord "trend break: touchpoint".
HANDLED_WITHIN_S = 6 * 3600
#: What the work runner hands to a person (work-needs-attention.txt) - its escalations, the
#: coordinator's notes - which touchpoints() did not read at all before.
_PERSON_KINDS = re.compile(r"^(BLOCKED|REVIEW_FAIL|ERROR|TIMEOUT|BOARD_UNREADABLE|NOTE|\w+_ESCALATE|\w+_NO_SESSION|"
                           r"DEFLAKE_(?!REQUESTED)\w+)$")
_ATT = re.compile(r"^(\d\d-\d\d \d\d:\d\d)\s+(\S+)\s+(\S+)\s+(\S+)")


def _handled(wlog: list[tuple[float, str]], ticket: str, branch: str, t: float) -> bool:
    marks = (f"FIX {ticket} attempt ", f"CONFLICT {ticket}: no fix run", f"QUEUED {branch} for merge")
    return any(t <= ts <= t + HANDLED_WITHIN_S and any(m in ln for m in marks) for ts, ln in wlog)


def touchpoints(ops: str, since: datetime, until: datetime) -> list[str]:
    """What a person had to do: attention lines that name a person's action, the work runner's
    escalations, and holds. A CONFLICT / GATE_FAIL the work runner took over is not one."""
    out = []
    wlog = []
    for ln in _read(os.path.join(ops, "work-runner.log")).splitlines():
        m = re.match(r"^\[(\d\d-\d\d \d\d:\d\d:\d\d)\] ", ln)
        if m:
            try:
                wlog.append((datetime.strptime(f"{since.year}-{m.group(1)}", "%Y-%m-%d %H:%M:%S").timestamp(), ln))
            except ValueError:
                pass
    for raw in _read(os.path.join(ops, "merge-needs-attention.txt")).splitlines():
        m = re.match(r"^\[?(\d\d-\d\d \d\d:\d\d)", raw)
        if not m:
            continue
        try:
            t = datetime.strptime(f"{since.year}-{m.group(1)}", "%Y-%m-%d %H:%M")
        except ValueError:
            continue
        if not since <= t <= until:
            continue
        a = _ATT.match(raw)
        if a and (a.group(4) == "GATE_FAIL" or a.group(4).startswith("CONFLICT(") or a.group(4) == "CONFLICT"):
            # younger than the window and not yet taken: pending, not yet a person's (a real
            # escalation arrives as its own work-needs line, counted below at once)
            if until.timestamp() - t.timestamp() >= HANDLED_WITHIN_S and not _handled(wlog, a.group(3), a.group(2), t.timestamp()):
                out.append(raw[:160] + "  (not taken by the work runner)")
            continue
        if re.search(r"CONFLICT|GATE_FAIL|SUITE_BR|FIX_HELD|BLOCKED|needs a person|a person must", raw):
            out.append(raw[:160])
    for raw in _read(os.path.join(ops, "work-needs-attention.txt")).splitlines():
        a = _ATT.match(raw)
        if not a or not _PERSON_KINDS.match(a.group(4)):
            continue
        try:
            t = datetime.strptime(f"{since.year}-{a.group(1)}", "%Y-%m-%d %H:%M")
        except ValueError:
            continue
        if since <= t <= until:
            out.append(raw[:160])
    for h in _jsonl(os.path.join(ops, "hold.jsonl")):
        t = datetime.fromtimestamp(float(h.get("ts", 0)))
        if since <= t <= until and h.get("event") == "hold":
            out.append(f"hold {h.get('minutes')} min: {h.get('why')}")
    return out


def flake_accepts(ops: str, since: datetime, until: datetime) -> list[dict]:
    """flaky.jsonl records the merge runner wrote when a red test passed alone twice and the gate went
    on without a re-run (the user's rule, 2026-09-23): {ts, tests, batch, suite, kind, saved_s}."""
    out = []
    for o in _jsonl(os.path.join(ops, "flaky.jsonl")):
        if not o.get("accepted"):
            continue
        try:
            t = datetime.fromisoformat(str(o.get("ts")))
        except ValueError:
            continue
        if since <= t <= until:
            out.append(dict(o, _t=t))
    return out


def summary(ops: str, now: datetime | None = None) -> dict:
    now = now or datetime.now()
    h6 = hourly(ops, now - timedelta(hours=6), now)
    h24 = hourly(ops, now - timedelta(hours=24), now)
    g24 = gate_rows(ops, now - timedelta(hours=24), now)
    closed = [g for g in g24 if g["verdict"] in ("green", "red")]
    reds = [g for g in closed if g["verdict"] == "red"]
    flakes = [g for g in closed if g["cause"] in ("flake", "flake-then-real")]
    full = [g["minutes"] for g in closed if g["class"] == "full" and g["minutes"]]
    full.sort()
    queue = [ln.strip() for ln in _read(os.path.join(ops, "merge-queue.txt")).splitlines()
             if ln.strip() and not ln.lstrip().startswith("#")]
    try:
        claims = json.load(open(os.path.join(ops, "work-claims.json")))
        running = sum(1 for c in claims.values() if c.get("state") == "running" and c.get("kind") == "work")
    except Exception:
        running = None
    cap = None
    for ev in reversed(parse_log(_read(os.path.join(ops, "work-runner.log")), now.year)[-400:]):
        m = re.search(r"cap=(\d+)", ev.text)
        if m and "VERSION" in ev.text:
            cap = int(m.group(1))
            break
    return {
        "ts": now.timestamp(), "at": now.strftime("%Y-%m-%d %H:%M"),
        "landings_per_h_6h": round(sum(r["landed"] for r in h6) / 6, 2),
        "landings_per_h_24h": round(sum(r["landed"] for r in h24) / 24, 2),
        "dispatch_24h": sum(r["dispatch"] for r in h24), "handback_24h": sum(r["handback"] for r in h24),
        "hours_with_dispatch_24h": sum(1 for r in h24 if r["dispatch"] > 0),
        "gate_occupancy_24h": round(sum(r["gate_min"] for r in h24) / (24 * 60), 2),
        "wait_min_24h": sum(r["wait_min"] for r in h24),
        "gates_24h": len(closed), "reds_24h": len(reds), "flakes_24h": len(flakes),
        "real_reds_24h": sum(1 for g in reds if g["cause"] in ("real", "flake-then-real")),
        "full_gate_p50_min": (full[len(full) // 2] if full else None),
        "conflicts_24h": sum(g["conflicts"] for g in g24),
        "touchpoints_24h": len(touchpoints(ops, now - timedelta(hours=24), now)),
        "queue_depth": len(queue), "workers_running": running, "worker_cap": cap,
        "flake_accepts_24h": len(fa := flake_accepts(ops, now - timedelta(hours=24), now)),
        "flake_saved_min_24h": round(sum(float(o.get("saved_s") or 0) for o in fa) / 60),
        # The one-solo-pass rule's own saving: the second isolated run it skipped (~ the first one's time).
        "flake_solo_24h": sum(1 for o in fa if o.get("passes_alone") == 1),
        "flake_solo_saved_min_24h": round(sum(float(o.get("solo_saved_s") or 0) for o in fa if o.get("passes_alone") == 1) / 60),
    }


def _cause(s: dict) -> str:
    """'reds 17/33 (7 real) · flakes 6'. `(real)` beside 17/33 read as 17 real reds when 7 were; and
    flakes are their own count, not a share of the reds - a gate that flaked and passed on retry is
    green, so '(7 real, 6 flake)' would not add up to 17 and would not be about the same gates."""
    real = f" ({s['real_reds_24h']} real)" if s["reds_24h"] else ""
    return real + (f" · flakes {s['flakes_24h']}" if s.get("flakes_24h") else "")


def summary_line(s: dict) -> str:
    cause = _cause(s)
    workers = "?" if s["workers_running"] is None else f"{s['workers_running']}/{s['worker_cap'] or '?'}"
    return (f"flow: {s['landings_per_h_6h']}/h (6h) {s['landings_per_h_24h']}/h (24h) · "
            f"reds {s['reds_24h']}/{s['gates_24h']}{cause} · conflicts {s['conflicts_24h']} · "
            f"touchpoints {s['touchpoints_24h']} · queue {s['queue_depth']} · workers {workers} · "
            f"dispatch-hours {s['hours_with_dispatch_24h']}/24 · gate {int(s['gate_occupancy_24h'] * 100)}%")


# --------------------------------------------------------------------- digest
# The user reads the numbers on his phone (2026-09-23: "visibility into pipeline improvements
# without waiting days"): the tick line goes to Discord every DIGEST_EVERY_S, and at once on a
# trend break. Breaks are judged against the flow.jsonl record nearest BREAK_LOOKBACK_S ago, so a
# 30-minute wobble in a 6 h rolling number is not a break.
DIGEST_EVERY_S = 2 * 3600
BREAK_LOOKBACK_S = 2 * 3600


def tick_line(ops: str, s: dict, now: datetime | None = None) -> str:
    """Invariant 23: `flow: <landings/h> · reds <n>/<gates> (<cause>) · touchpoints <n> ·
    <experiment id> gate <k>/<n> · holding: <none|until hh:mm why>`."""
    now = now or datetime.now()
    try:
        from hkpy import experiment              # lazy: experiment imports this module
        cur, m, checks = experiment.status_of(ops, now)
        if cur:
            broken = [t for ok, t in checks if ok is False]
            exp = f"{cur['id']} gate {m['gates']}/{cur['gates']}" + (f" GUARD BROKEN: {'; '.join(broken)}" if broken else "")
        else:
            exp = "no experiment"
    except Exception as e:                       # the digest must not die on a ledger problem
        exp = f"experiment ? ({type(e).__name__})"
    return (f"flow: {s['landings_per_h_6h']}/h (6h) {s['landings_per_h_24h']}/h (24h) · "
            f"reds {s['reds_24h']}/{s['gates_24h']}{_cause(s)} · touchpoints {s['touchpoints_24h']} · "
            f"{exp} · holding: {_holding(ops, now)}"
            + (f" · {s['open_graph']['short']}" if (s.get("open_graph") or {}).get("short") else "")
            + (f" · flake-accepts {s['flake_accepts_24h']} (saved {s['flake_saved_min_24h']} min"
               + (f"; {s['flake_solo_24h']} after one solo pass, {s['flake_solo_saved_min_24h']} min of it" if s.get("flake_solo_24h") else "")
               + ")"
               if s.get("flake_accepts_24h") else ""))


def _holding(ops: str, now: datetime) -> str:
    rec: dict = {}
    try:
        for ln in open(os.path.join(ops, "hold"), encoding="utf-8"):
            k, _, v = ln.strip().partition("=")
            rec[k] = v
        until = float(rec.get("until", 0))
    except (OSError, ValueError):
        return "none"
    if until <= now.timestamp():
        return "none"
    return f"until {datetime.fromtimestamp(until).strftime('%H:%M')} {rec.get('why', '')}".rstrip()


def trend_breaks(s: dict, records: list[dict], holding: bool) -> list[tuple[str, str]]:
    """[(kind, why)] for this summary against the record nearest BREAK_LOOKBACK_S before it.
    The role's list: landings/h halves, real red rate doubles, a touchpoint appears, a hold is
    written. Small numbers are not trends: halving needs a prior rate of at least 0.5/h, doubling
    needs at least +2 real reds."""
    prior = [r for r in records if isinstance(r, dict) and r.get("ts", 0) <= s["ts"] - BREAK_LOOKBACK_S]
    ref = max(prior, key=lambda r: r["ts"]) if prior else None
    last = max((r for r in records if isinstance(r, dict) and r.get("ts", 0) < s["ts"]), key=lambda r: r["ts"], default=None)
    out = []
    if ref:
        a, b = ref.get("landings_per_h_6h") or 0, s["landings_per_h_6h"]
        if a >= 0.5 and b <= a / 2:
            out.append(("landings", f"landings/h (6h) halved: {a} -> {b} since {ref.get('at')}"))
        a, b = ref.get("real_reds_24h") or 0, s["real_reds_24h"]
        if b >= 2 * a and b >= a + 2:
            out.append(("reds", f"real reds (24h) doubled: {a} -> {b} since {ref.get('at')}"))
    if last and s["touchpoints_24h"] > (last.get("touchpoints_24h") or 0):
        out.append(("touchpoint", f"touchpoints (24h) {last.get('touchpoints_24h')} -> {s['touchpoints_24h']}: a person had to act"))
    if holding:
        out.append(("hold", "the merge queue is held"))
    return out


def _last_sent(ops: str, key: str) -> float:
    last = 0.0
    for o in _jsonl(os.path.join(ops, "alerts.jsonl"))[-2000:]:
        if o.get("key") == key and o.get("status") == "sent":
            last = max(last, float(o.get("ts", 0)))
    return last


def eta_line(ops: str, now: datetime, repo: str | None = None) -> str:
    """"ETA: queue clears ~HH:MM; T-801 lands ~HH:MM" (hkpy.eta) from measured medians. Never raises."""
    import statistics
    from hkpy import eta, taskorder
    repo = repo or os.environ.get("HACKRIFF_REPO") or "/Users/daniellewis/hackriff"
    queue = [ln.strip() for ln in _read(os.path.join(ops, "merge-queue.txt")).splitlines()
             if ln.strip() and not ln.lstrip().startswith("#")]
    started = None
    for ln in _read(os.path.join(ops, "bulk-in-progress")).splitlines():
        if ln.startswith("started="):
            try:
                started = datetime.strptime(ln.split("=", 1)[1].strip(), "%Y-%m-%d %H:%M:%S")
            except ValueError:
                pass
    if started is None and os.path.exists(os.path.join(repo, ".git", "MERGE_HEAD")):
        started = datetime.fromtimestamp(os.path.getmtime(os.path.join(repo, ".git", "MERGE_HEAD")))
    full = sorted(r["minutes"] for r in gate_rows(ops, now - timedelta(hours=24), now)
                  if r["class"] == "full" and r["verdict"] == "green" and r["minutes"])
    gate_min = float(statistics.median(full)) if full else 25.0
    done = _jsonl(os.path.join(ops, "work-done.jsonl"))
    work = [float(o["minutes"]) for o in done if o.get("kind") == "work" and o.get("outcome") in ("done", "done-to-review") and o.get("minutes")][-40:]
    review = [float(o["minutes"]) for o in done if o.get("kind") == "review" and o.get("minutes")][-40:]
    work_min = statistics.median(work) if work else 25.0
    review_min = statistics.median(review) if review else 2.0
    q_eta = eta.queue_clears(now, len(queue), started, gate_min)
    ticket, t_eta, why = None, None, ""
    try:
        tasks, _ = taskorder.committed_tasks(repo, ops)
        a = taskorder.analyse(tasks)
        if a["roots"]:
            top = a["roots"][0]
            ticket = top["id"]
            try:
                claim = json.load(open(os.path.join(ops, "work-claims.json"))).get(ticket)
            except Exception:
                claim = None
            branch = "task-t" + ticket.split("-", 1)[1].lstrip("0")
            t_eta, why = eta.ticket_lands(now, ticket, branch, queue, claim, started, gate_min, work_min,
                                          review_min, board_status=top["status"])
            why = f"unblocks {top['unblocks']}; {why}"
    except Exception as e:
        ticket, why = None, f"({type(e).__name__})"
    return eta.digest_line(now, q_eta, len(queue), ticket, t_eta, why)


def open_graph(ops: str, now: datetime, s: dict, record: bool = False, repo: str | None = None) -> dict:
    """hkpy.graphclear over main's committed board: {line, short, n, at}; with `record`, one ETA-ledger
    line each for "queue clears" and "open graph clears". Never raises."""
    from hkpy import eta, graphclear
    repo = repo or os.environ.get("HACKRIFF_REPO") or "/Users/daniellewis/hackriff"
    try:
        r = graphclear.gather(ops, repo, now, float(s.get("landings_per_h_6h") or 0), float(s.get("landings_per_h_24h") or 0))
    except Exception as e:
        return {"line": f"open graph: no estimate ({type(e).__name__}: {e})"[:200], "short": ""}
    at = (r.get("eta", {}).get("p50") or {}).get("at")
    out = {"line": r["line"], "n": r.get("n"), "at": at, "chain": r.get("chain"), "bound": r.get("bound"),
           "short": f"graph clears ~{graphclear._when(at, now)} ({r.get('n')})" if at else ""}
    if record:
        try:
            queue = [ln.strip() for ln in _read(os.path.join(ops, "merge-queue.txt")).splitlines() if ln.strip()]
            gating = os.path.exists(os.path.join(ops, "bulk-in-progress")) or os.path.exists(os.path.join(repo, ".git", "MERGE_HEAD"))
            full = sorted(g["minutes"] for g in gate_rows(ops, now - timedelta(hours=24), now)
                          if g["class"] == "full" and g["verdict"] == "green" and g["minutes"])
            gate_min = float(full[len(full) // 2]) if full else 25.0
            started = None
            for ln in _read(os.path.join(ops, "bulk-in-progress")).splitlines():
                if ln.startswith("started="):
                    try:
                        started = datetime.strptime(ln.split("=", 1)[1].strip(), "%Y-%m-%d %H:%M:%S")
                    except ValueError:
                        pass
            graphclear.record(ops, now, r, eta.queue_clears(now, len(queue), started, gate_min), len(queue), gating)
        except Exception:
            pass
    return out


def digest(ops: str, s: dict, now: datetime | None = None, send=None) -> list[str]:
    """Post the tick line when due, and each trend break at once. Returns what was posted (keys).
    `send(level, title, body, key)` defaults to ops/alert.py, which dedupes by key for 30 min and
    never raises; a break uses its own key so it is never swallowed by the 2 h digest."""
    now = now or datetime.now()
    if send is None:
        from hkpy.knobs import alert as send
    line = tick_line(ops, s, now)
    posted = []
    holding = _holding(ops, now) != "none"
    for kind, why in trend_breaks(s, _jsonl(os.path.join(ops, "flow.jsonl")), holding):
        send("amber", f"pipeline trend break: {kind}", f"{why}\n{line}", f"flow:break:{kind}")
        posted.append(f"flow:break:{kind}")
    last = _last_sent(ops, "flow:digest")
    if now.timestamp() - last >= DIGEST_EVERY_S:
        # Every flake the gate accepted since the last digest, by name - never a silent pass.
        events = flake_accepts(ops, datetime.fromtimestamp(last) if last else now - timedelta(hours=24), now)
        body = line + "".join(
            f"\nflake accepted {o['_t'].strftime('%H:%M')}: {o.get('tests')} in `just {o.get('suite')}` "
            f"({o.get('batch')}) - passed alone twice, ~{round(float(o.get('saved_s') or 0) / 60)} min saved"
            for o in events)
        # ...why tickets were handed back for a fix run, per class (user, 2026-09-24)
        try:
            from hkpy import fixes
            body += "\n" + fixes.tally_line(fixes.rows(ops, now.timestamp() - 86400))
        except Exception:
            pass
        # ...and what landed since the last one, as release notes (user, 2026-09-23)
        since = last or now.timestamp() - DIGEST_EVERY_S
        landed = [o for o in _jsonl(os.path.join(ops, "landed.jsonl")) if float(o.get("merge_ts") or 0) > since]
        if landed:
            try:
                from hkpy import landnotes
                body += "\n\n" + landnotes.notes(f"landed since last digest: {len(landed)}",
                                                  [str(o.get("branch")) for o in landed], ops=ops, limit=1900 - len(body))
            except Exception:
                body += f"\n\nlanded since last digest: {len(landed)}"
        try:
            extra = eta_line(ops, now)
            if (s.get("open_graph") or {}).get("line"):
                from hkpy import graphclear
                extra += "\n" + s["open_graph"]["line"] + "\n" + graphclear.accuracy_line(ops)
            body = body.replace(line, line + "\n" + extra, 1)
        except Exception:
            pass
        send("green", "pipeline digest", body, "flow:digest")
        posted.append("flow:digest")
    return posted


def _parse_since(text: str, now: datetime) -> datetime:
    m = re.fullmatch(r"(\d+)([hd])", text)
    if m:
        n, unit = int(m.group(1)), m.group(2)
        return now - (timedelta(hours=n) if unit == "h" else timedelta(days=n))
    return datetime.fromisoformat(text)


def _table(rows: list[dict]) -> str:
    if not rows:
        return "(no rows)"
    cols = list(rows[0].keys())
    w = {c: max(len(c), *(len(str(r.get(c, ""))) for r in rows)) for c in cols}
    lines = ["  ".join(c.ljust(w[c]) for c in cols)]
    for r in rows:
        lines.append("  ".join(str("" if r.get(c) is None else r.get(c)).ljust(w[c]) for c in cols))
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="just flow", description=__doc__.split("\n\n")[0])
    p.add_argument("--hourly", action="store_true", help="per-hour occupancy table (default)")
    p.add_argument("--gates", action="store_true", help="one row per gate")
    p.add_argument("--tickets", action="store_true", help="one row per landed ticket")
    p.add_argument("--touchpoints", action="store_true", help="what a person had to do")
    p.add_argument("--since", default="24h", help="24h | 6h | 2d | ISO datetime (default 24h)")
    p.add_argument("--record", action="store_true", help="append the summary to $HACKRIFF_OPS/flow.jsonl")
    p.add_argument("--digest", action="store_true",
                   help="post the tick line to Discord if 2 h have passed, and any trend break now (run after --record)")
    p.add_argument("--json", action="store_true")
    p.add_argument("--ops", default=OPS)
    a = p.parse_args(argv)
    now = datetime.now()
    since = _parse_since(a.since, now)
    views = []
    if a.gates:
        views.append(("gates", gate_rows(a.ops, since, now)))
    if a.tickets:
        views.append(("tickets", ticket_rows(a.ops, since, now)))
    if a.touchpoints:
        views.append(("touchpoints", [{"touchpoint": t} for t in touchpoints(a.ops, since, now)]))
    if a.hourly or not views:
        views.insert(0, ("hourly", hourly(a.ops, since, now)))
    s = summary(a.ops, now)
    if a.record or a.digest:
        s["open_graph"] = open_graph(a.ops, now, s, record=a.record)
    if a.json:
        print(json.dumps({"summary": s, **{k: v for k, v in views}}, indent=1))
    else:
        for name, rows in views:
            print(f"== {name} since {since.strftime('%m-%d %H:%M')}")
            print(_table(rows))
            print()
        print(summary_line(s))
    if a.record:
        os.makedirs(a.ops, exist_ok=True)
        with open(os.path.join(a.ops, "flow.jsonl"), "a", encoding="utf-8") as fh:
            fh.write(json.dumps(s) + "\n")
    if a.digest:
        print(tick_line(a.ops, s, now))
        for k in digest(a.ops, s, now):
            print(f"digest: posted {k}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
