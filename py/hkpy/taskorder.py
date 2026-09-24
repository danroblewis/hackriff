"""`just task order` - which open tickets hold the most work behind them. Read-only over the board.

WHY (user, 2026-09-23). Twice that day dispatch sat at 1 of 6 workers with a "frontier" of zero,
and the answer - T-801 holds 19 MMAP tickets, T-565 the MAUTO chain - was found by counting
`depends_on` by hand. This computes it:

  * **unblocks N** - how many OPEN tickets are transitively behind a ticket (via depends_on/deps);
  * **moves** - the milestones those tickets (and it) belong to;
  * **the frontier** - todo tickets whose every dependency is done or cancelled, not blocked and not
    waiting on a person or hardware: what a dispatcher could start right now;
  * **a topological order** of the open tickets (dependencies first; a cycle is reported, not hidden).

The ranking is of ROOT bottlenecks by default: open tickets not themselves waiting on another open
ticket, so the list names what to act on, not every link of a chain. Pure over the parsed board -
`py/hkpy/tasks.py order` and the dashboard (`/taskorder.json`, ops/monitor.py) feed it the text.
"""

from __future__ import annotations

from collections import defaultdict, deque

CLOSED = ("done", "cancelled")


def _deps(t: dict) -> list[str]:
    return [str(d) for d in (t.get("depends_on") or t.get("deps") or [])]


def analyse(tasks: list[dict]) -> dict:
    by_id = {str(t.get("id")): t for t in tasks if t.get("id")}
    open_ = {i: t for i, t in by_id.items() if t.get("status") not in CLOSED}
    waits_on = {i: [d for d in _deps(t) if d in open_] for i, t in open_.items()}
    behind = defaultdict(list)                      # d -> open tickets that depend on d directly
    for i, ds in waits_on.items():
        for d in ds:
            behind[d].append(i)

    def descendants(i: str) -> set[str]:
        seen, todo = set(), list(behind.get(i, []))
        while todo:
            j = todo.pop()
            if j not in seen:
                seen.add(j)
                todo.extend(behind.get(j, []))
        return seen

    rows = []
    for i, t in open_.items():
        ds = descendants(i)
        rows.append({
            "id": i, "title": str(t.get("title") or ""), "status": t.get("status"),
            "milestone": t.get("milestone"), "unblocks": len(ds),
            "moves": sorted({str(open_[j].get("milestone")) for j in ds | {i} if open_[j].get("milestone")}),
            "waits_on": waits_on[i], "root": not waits_on[i],
            "blocked": bool(t.get("blocked_on")) or t.get("needs") in ("user", "hardware"),
        })
    rows.sort(key=lambda r: (-r["unblocks"], r["id"]))

    # todo, nothing open ahead of it, not blocked or waiting on a person/hardware. A dependency
    # missing from the board counts as satisfied - the board's validator owns dangling references.
    frontier = [r["id"] for r in rows if r["status"] == "todo" and not r["waits_on"] and not r["blocked"]]

    indeg = {i: len(ds) for i, ds in waits_on.items()}
    q = deque(sorted(i for i, n in indeg.items() if n == 0))
    order = []
    while q:
        i = q.popleft()
        order.append(i)
        for j in sorted(behind.get(i, [])):
            indeg[j] -= 1
            if indeg[j] == 0:
                q.append(j)
    cycle = sorted(i for i, n in indeg.items() if n > 0)
    # A ticket listing ITSELF can never dispatch - the likeliest cycle, named on its own
    # (T-568, 2026-09-23, holding T-576/T-577 behind it).
    self_deps = sorted(i for i, t in open_.items() if i in _deps(t))
    return {"rows": rows, "roots": [r for r in rows if r["root"] and r["unblocks"] > 0],
            "frontier": frontier, "order": order, "cycle": cycle, "self_deps": self_deps, "open": len(open_)}


def render(a: dict, top: int = 10, show_order: bool = False) -> list[str]:
    out = [f"task order: {a['open']} open · frontier {len(a['frontier'])} dispatchable now"
           + (f" · CYCLE: {' '.join(a['cycle'])}" if a["cycle"] else "")
           + (f" (self-dependency: {' '.join(a['self_deps'])})" if a.get("self_deps") else "")]
    out.append(f"{'unblocks':>8}  {'ticket':<7} {'status':<12} moves        title")
    for r in a["roots"][:top]:
        why = " (blocked)" if r["blocked"] else ""
        out.append(f"{r['unblocks']:>8}  {r['id']:<7} {str(r['status']) + why:<12} {','.join(r['moves'])[:12]:<12} {r['title'][:70]}")
    if not a["roots"]:
        out.append("       -  nothing open holds another open ticket")
    out.append("frontier: " + (" ".join(a["frontier"][:30]) + (" …" if len(a["frontier"]) > 30 else "") or "(empty)"))
    if show_order:
        out.append("topological order: " + " ".join(a["order"]))
    return out
