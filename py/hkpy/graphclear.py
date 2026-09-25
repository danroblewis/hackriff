"""When does the open ticket graph clear? (user, 2026-09-24) - read-only over the board and the ops logs.

The headline beside "queue clears": **open graph clears ~HH:MM (N tickets)** = the latest expected
landing over every todo / in-progress ticket, EXCLUDING what cannot land without someone outside the
pipeline - deferred, blocked (status blocked, a `blocked_on`, `needs: user|hardware`), anything that
depends on one of those, and a dependency cycle - each counted with its reason.

Two bounds, and the later one is the answer (and is named):

  * **the dependency chain** - each ticket's expected landing is
      queued branch                 -> the queue estimate (hkpy.eta.ticket_lands)
      dispatched, deps all landed   -> now + the remaining time of samples that ran longer than it has
      otherwise                     -> max(now, latest landing of its open deps) + d
    where d is one ticket's measured **dispatch -> landed** time (work-runner.log DISPATCH to
    landed.jsonl merge, the last 7 days). A dependent ticket is dispatched only after its dep lands,
    so a chain's durations add. The chain that reaches the latest time is the critical path.
  * **throughput** - N tickets at the measured landings/h (the dispatch cap, not the graph, limits a
    wide frontier).

The band re-runs both with the p25 and p75 of the same duration samples (throughput with the faster
and slower of the 6 h and 24 h landing rates). Estimates, marked "~": reds, flakes and review rounds
live inside the samples, they are not modelled separately.

The ETA ledger ($HACKRIFF_OPS/eta-ledger.jsonl) keeps every tick's prediction of this number and of
"queue clears", and `accuracy` resolves each one against the first later tick that saw it happen.
"""

from __future__ import annotations

import json
import os
import re
import statistics
from datetime import datetime, timedelta

SCOPE = ("todo", "in-progress")
LEDGER = "eta-ledger.jsonl"
SAMPLE_DAYS = 7


def _deps(t: dict) -> list[str]:
    return [str(d) for d in (t.get("depends_on") or t.get("deps") or [])]


def _outside(t: dict) -> str:
    """Why a ticket cannot land from inside the pipeline, or ""."""
    if t.get("status") == "deferred":
        return "deferred"
    if t.get("status") == "blocked" or t.get("blocked_on") or t.get("needs") in ("user", "hardware"):
        return "blocked (needs user/hardware)"
    return ""


def _remaining(samples: list[float], elapsed: float, label: str, d: float) -> float:
    """Minutes left for a ticket dispatched `elapsed` minutes ago: the same quantile of the samples
    that ran longer than that, minus what has passed (an overdue ticket - T-801, review-failed seven
    hours in - is not "landing now"). Fewer than 2 such samples: a fresh ticket's d."""
    longer = sorted(x - elapsed for x in samples if x > elapsed)
    if len(longer) < 2:
        return d
    return statistics.quantiles(longer, n=4)[("p25", "p50", "p75").index(label)]


def estimate(tasks: list[dict], now: datetime, samples: list[float], dispatched: dict[str, float],
             queued: dict[str, datetime], rate6: float, rate24: float) -> dict:
    """Pure. `samples` = dispatch->landed minutes; `dispatched` = ticket -> epoch of its latest
    DISPATCH (in-flight tickets only); `queued` = ticket -> queue estimate for a queued branch."""
    by = {str(t.get("id")): t for t in tasks if t.get("id")}
    live = {i: t for i, t in by.items() if t.get("status") in SCOPE + ("deferred", "blocked")}
    excluded: dict[str, list[str]] = {}
    why: dict[str, str] = {i: _outside(t) for i, t in live.items()}
    # anything waiting (transitively) on an excluded ticket cannot land either
    changed = True
    while changed:
        changed = False
        for i, t in live.items():
            if not why[i] and any(why.get(d) for d in _deps(t) if d in live):
                why[i] = "waits on a deferred/blocked ticket"
                changed = True
    scope = {i: t for i, t in live.items() if not why[i]}
    # a cycle can never be scheduled: Kahn's order over the in-scope graph
    indeg = {i: sum(1 for d in _deps(t) if d in scope) for i, t in scope.items()}
    order, ready = [], sorted(i for i, n in indeg.items() if n == 0)
    while ready:
        i = ready.pop(0)
        order.append(i)
        for j in sorted(scope):
            if i in _deps(scope[j]):
                indeg[j] -= 1
                if indeg[j] == 0:
                    ready.append(j)
    for i in scope:
        if i not in order:
            why[i] = "dependency cycle (or waits on one)"
    scope = {i: scope[i] for i in order}
    for i, w in why.items():
        if w:
            excluded.setdefault(w, []).append(i)
    n = len(scope)
    out = {"n": n, "excluded": {k: sorted(v) for k, v in sorted(excluded.items())},
           "samples": len(samples), "rates": [rate6, rate24], "eta": {}, "chain": [], "bound": None}
    if not n:
        return out
    if not samples:
        out["error"] = "no dispatch->landed samples"
        return out
    qs = statistics.quantiles(samples, n=4) if len(samples) >= 2 else [samples[0]] * 3
    for label, d, rate in (("p25", qs[0], max(rate6, rate24)), ("p50", qs[1], rate24), ("p75", qs[2], min(rate6, rate24))):
        ef, via = {}, {}
        for i in order:
            deps = [x for x in _deps(scope[i]) if x in scope]
            start = max([now] + [ef[x] for x in deps])
            via[i] = max(deps, key=lambda x: ef[x]) if deps and max(ef[x] for x in deps) > now else None
            if i in queued:
                ef[i] = max(queued[i], start)
            elif i in dispatched and not deps:
                ef[i] = now + timedelta(minutes=_remaining(samples, (now.timestamp() - dispatched[i]) / 60, label, d))
            else:
                ef[i] = start + timedelta(minutes=d)
        last = max(order, key=lambda i: ef[i])
        chain = [last]
        while via.get(chain[-1]):
            chain.append(via[chain[-1]])
        chain_eta = ef[last]
        thr_eta = now + timedelta(hours=n / rate) if rate > 0 else None
        bound = "throughput" if thr_eta and thr_eta > chain_eta else "chain"
        out["eta"][label] = {"at": max(chain_eta, thr_eta or chain_eta).timestamp(), "chain_at": chain_eta.timestamp(),
                             "throughput_at": thr_eta.timestamp() if thr_eta else None, "d_min": round(d), "rate": rate}
        if label == "p50":
            out["chain"], out["bound"] = list(reversed(chain)), bound
    return out


def _when(ts: float, now: datetime) -> str:
    t = datetime.fromtimestamp(ts)
    return t.strftime("%H:%M") if t.date() == now.date() else t.strftime("%a %H:%M")


def line(r: dict, now: datetime) -> str:
    ex = r.get("excluded") or {}
    ex_txt = (f"excluded {sum(len(v) for v in ex.values())}: " + ", ".join(f"{k} {len(v)}" for k, v in ex.items())) if ex else "excluded 0"
    if not r.get("n"):
        return f"open graph: nothing in scope ({ex_txt})"
    if r.get("error") or "p50" not in r.get("eta", {}):
        return f"open graph: {r['n']} tickets, no estimate ({r.get('error', 'no data')}); {ex_txt}"
    e = r["eta"]
    chain = " -> ".join(r["chain"])
    how = (f"set by throughput {e['p50']['rate']}/h; longest chain {chain} ~{_when(e['p50']['chain_at'], now)}"
           if r["bound"] == "throughput" else
           f"set by the chain {chain} ({len(r['chain'])} tickets; a fresh one's p50 is {e['p50']['d_min'] / 60:.1f} h)")
    return (f"open graph clears ~{_when(e['p50']['at'], now)} ({r['n']} tickets; p25-p75 "
            f"~{_when(e['p25']['at'], now)}-{_when(e['p75']['at'], now)}) - {how}; {ex_txt}")


# ------------------------------------------------------------------ IO
_DISPATCH = re.compile(r"^\[(\d\d)-(\d\d) (\d\d):(\d\d):(\d\d)\] DISPATCH (T-\d+)")


def dispatch_times(log_text: str, year: int) -> dict[str, list[float]]:
    out: dict[str, list[float]] = {}
    for ln in log_text.splitlines():
        m = _DISPATCH.match(ln)
        if m:
            try:
                t = datetime(year, *map(int, m.groups()[:5])).timestamp()
            except ValueError:
                continue
            out.setdefault(m.group(6), []).append(t)
    return out


def _ticket_of(o: dict) -> str | None:
    tk = str(o.get("ticket") or "")
    if tk.startswith("T-"):
        return tk
    m = re.match(r"task-t(\d+)", str(o.get("branch") or ""))
    return f"T-{m.group(1)}" if m else None


def samples_from(disp: dict[str, list[float]], landed: list[dict], now: datetime, days: int = SAMPLE_DAYS) -> list[float]:
    out = []
    for o in landed:
        tid, merged = _ticket_of(o), float(o.get("merge_ts") or 0)
        if not tid or merged < now.timestamp() - days * 86400:
            continue
        before = [t for t in disp.get(tid, []) if t < merged]
        if before:
            out.append((merged - max(before)) / 60)
    return out


def gather(ops: str, repo: str, now: datetime, rate6: float, rate24: float) -> dict:
    from hkpy import eta, flow, taskorder
    tasks, ref = taskorder.committed_tasks(repo, ops)
    try:
        wlog = open(os.path.join(ops, "work-runner.log"), encoding="utf-8", errors="replace").read()
    except OSError:
        wlog = ""
    disp = dispatch_times(wlog, now.year)
    landed = flow._jsonl(os.path.join(ops, "landed.jsonl"))
    try:
        claims = json.load(open(os.path.join(ops, "work-claims.json")))
    except Exception:
        claims = {}
    inflight = {tid for tid, c in claims.items() if tid.startswith("T-") and c.get("state") not in ("no-work", "error", "timeout")}
    dispatched = {tid: max(ts) for tid, ts in disp.items() if tid in inflight}
    queue = [ln.strip() for ln in flow._read(os.path.join(ops, "merge-queue.txt")).splitlines() if ln.strip()]
    queued = {}
    for b in queue:
        m = re.match(r"task-t(\d+)", b)
        if m:
            q, _ = eta.ticket_lands(now, f"T-{m.group(1)}", b, queue, None, None, 30.0, 0, 0)
            if q:
                queued[f"T-{m.group(1)}"] = q
    r = estimate(tasks, now, samples_from(disp, landed, now), dispatched, queued, rate6, rate24)
    r["board"] = ref
    r["line"] = line(r, now)
    return r


def record(ops: str, now: datetime, r: dict, q_eta: datetime | None, depth: int, gating: bool) -> None:
    """One ledger line per tick for each number: what was predicted, and what was true now."""
    rows = [{"ts": now.timestamp(), "kind": "queue_clears", "predicted": q_eta.timestamp() if q_eta else None,
             "state": depth + int(gating)},
            {"ts": now.timestamp(), "kind": "open_graph", "state": r.get("n", 0),
             "predicted": (r.get("eta", {}).get("p50") or {}).get("at"),
             "p25": (r.get("eta", {}).get("p25") or {}).get("at"), "p75": (r.get("eta", {}).get("p75") or {}).get("at"),
             "bound": r.get("bound"), "chain": r.get("chain")}]
    with open(os.path.join(ops, LEDGER), "a", encoding="utf-8") as fh:
        for row in rows:
            fh.write(json.dumps(row) + "\n")


def accuracy(rows: list[dict], kind: str) -> dict:
    """Resolve each prediction against the first later record of the same kind whose state is 0
    (queue empty and nothing gating / no in-scope ticket left). error = actual - predicted, minutes."""
    rs = sorted((r for r in rows if r.get("kind") == kind), key=lambda r: r["ts"])
    errs, pending = [], 0
    for k, r in enumerate(rs):
        if not r.get("predicted") or not r.get("state"):
            continue
        hit = next((x for x in rs[k + 1:] if x.get("state") == 0), None)
        if hit is None:
            pending += 1
        else:
            errs.append((hit["ts"] - r["predicted"]) / 60)
    return {"resolved": len(errs), "pending": pending,
            "median_error_min": round(statistics.median(errs), 1) if errs else None}


def accuracy_line(ops: str) -> str:
    rows = []
    try:
        for ln in open(os.path.join(ops, LEDGER), encoding="utf-8"):
            try:
                rows.append(json.loads(ln))
            except ValueError:
                pass
    except OSError:
        pass
    parts = []
    for kind, name in (("queue_clears", "queue-clears"), ("open_graph", "open graph")):
        a = accuracy(rows, kind)
        err = f"median error {a['median_error_min']:+} min over {a['resolved']}" if a["resolved"] else "none resolved yet"
        parts.append(f"{name}: {err}, {a['pending']} pending")
    return "ETA ledger - " + "; ".join(parts)
