"""`just task order` - leverage: which open tickets release the most when they land. Read-only.

WHY (user, 2026-09-23). Twice that day dispatch sat at 1 of 6 workers with an empty frontier, and
the answer - T-801 holds 19 MMAP tickets, T-565 the MAUTO chain - was found by counting
`depends_on` by hand. For every OPEN ticket (todo / blocked / in-progress / deferred) this gives:

  * **depth** - 0 = nothing open ahead of it (ready now, or already being worked), 1 = one landing
    away (every open dependency is depth 0), 2 = two away, ...; `None` inside a dependency cycle;
  * **gate** - what it waits on: its open dependencies, and any `blocked_on` / `needs` (a person,
    hardware);
  * **unblocks N** - open tickets transitively downstream of it via depends_on/deps;
  * **value** - what landing it releases: the sum of WEIGHT over that downstream set, where
        weight = 3 if user-requested, else 2 high / 1 normal (also medium, unset) / 0.5 low,
                 + 0.5 per use-case id on the ticket
    ("user-requested" is the work runner's rule: requested_by / user_report / found_by "user...",
    or a priority that says "(user)"); shown with the downstream milestones, e.g. MMAP 12;
  * the **frontier** - depth 0, todo, not blocked or waiting on a person/hardware: dispatchable now;
  * a topological order (dependencies first), cycles and self-dependencies reported, never hidden.

Pure over the parsed board. `py/hkpy/tasks.py order` reads main's COMMITTED board (the bulk
marker's base while a batch gates - main's tip is provisional then); ops/monitor.py serves the
same analysis as /taskorder.json for the Leverage panel on /worklog.
"""

from __future__ import annotations

import os
import re
import subprocess
from collections import Counter, defaultdict, deque

OPEN = ("todo", "blocked", "in-progress", "deferred")
PRIORITY_WEIGHT = {"high": 2.0, "normal": 1.0, "medium": 1.0, "low": 0.5}
FORMULA = ("value = sum over the open tickets transitively downstream (via depends_on) of a weight: "
           "3 if user-requested, else high 2 / normal 1 / low 0.5, plus 0.5 per use-case id")


def _deps(t: dict) -> list[str]:
    return [str(d) for d in (t.get("depends_on") or t.get("deps") or [])]


def is_user(t: dict) -> bool:
    fb = str(t.get("found_by", ""))[:40].lower()
    return bool(t.get("requested_by") or t.get("user_report") or fb.startswith("user")
                or re.search(r"\(user\)", str(t.get("priority") or ""), re.I))


def weight(t: dict) -> float:
    base = 3.0 if is_user(t) else PRIORITY_WEIGHT.get(str(t.get("priority") or "normal").lower(), 1.0)
    return base + 0.5 * len(t.get("use_cases") or [])


def analyse(tasks: list[dict]) -> dict:
    by_id = {str(t.get("id")): t for t in tasks if t.get("id")}
    open_ = {i: t for i, t in by_id.items() if t.get("status") in OPEN}
    waits_on = {i: [d for d in _deps(t) if d in open_] for i, t in open_.items()}
    behind = defaultdict(list)
    for i, ds in waits_on.items():
        for d in ds:
            behind[d].append(i)

    def downstream(i: str) -> set[str]:
        seen, todo = set(), list(behind.get(i, []))
        while todo:
            j = todo.pop()
            if j not in seen:
                seen.add(j)
                todo.extend(behind.get(j, []))
        seen.discard(i)
        return seen

    depth: dict[str, int | None] = {}

    def depth_of(i: str, path: frozenset = frozenset()) -> int | None:
        if i in depth:
            return depth[i]
        if i in path:
            return None
        ds = [depth_of(d, path | {i}) for d in waits_on[i]]
        depth[i] = None if any(d is None for d in ds) else (1 + max(ds) if ds else 0)
        return depth[i]

    rows = []
    for i, t in open_.items():
        ds = downstream(i)
        gate = list(waits_on[i])
        if t.get("blocked_on"):
            gate.append(f"blocked: {t['blocked_on']}")
        if t.get("needs") in ("user", "hardware"):
            gate.append(f"needs {t['needs']}")
        ms = Counter(str(open_[j].get("milestone") or "?") for j in ds)
        rows.append({
            "id": i, "title": str(t.get("title") or ""), "status": t.get("status"),
            "milestone": t.get("milestone"), "depth": depth_of(i), "gate": gate,
            "waits_on": waits_on[i], "unblocks": len(ds),
            "value": round(sum(weight(open_[j]) for j in ds), 1),
            "downstream_milestones": dict(sorted(ms.items(), key=lambda kv: (-kv[1], kv[0]))),
            "moves": sorted({str(open_[j].get("milestone")) for j in ds | {i} if open_[j].get("milestone")}),
            "weight": weight(t), "root": not waits_on[i],
            "blocked": bool(t.get("blocked_on")) or t.get("needs") in ("user", "hardware") or t.get("status") == "blocked",
        })
    rows.sort(key=lambda r: (-r["unblocks"], -r["value"], r["id"]))

    frontier = [r["id"] for r in rows if r["depth"] == 0 and r["status"] == "todo" and not r["blocked"]]

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
    groups: dict[str, list[str]] = defaultdict(list)
    for i in order + cycle:
        d = depth.get(i)
        groups["cycle" if d is None else str(d)].append(i)
    return {"formula": FORMULA, "rows": rows, "roots": [r for r in rows if r["root"] and r["unblocks"] > 0],
            "frontier": frontier, "order": order, "groups": dict(groups), "cycle": cycle,
            "self_deps": sorted(i for i, t in open_.items() if i in _deps(t)), "open": len(open_)}


def _ms(r: dict) -> str:
    return ", ".join(f"{k} {v}" for k, v in r["downstream_milestones"].items())


def render(a: dict, top: int = 10, show_order: bool = False, sort: str = "unblocks") -> list[str]:
    key = (lambda r: (-r["value"], -r["unblocks"], r["id"])) if sort == "value" else (lambda r: (-r["unblocks"], -r["value"], r["id"]))
    out = [f"task order: {a['open']} open · frontier {len(a['frontier'])} dispatchable now"
           + (f" · CYCLE: {' '.join(a['cycle'])}" if a["cycle"] else "")
           + (f" (self-dependency: {' '.join(a['self_deps'])})" if a.get("self_deps") else ""),
           f"  ({a['formula']})",
           f"{'unblocks':>8} {'value':>6}  {'ticket':<7} {'status':<12} {'depth':>5}  downstream milestones / title"]
    shown = [r for r in sorted(a["rows"], key=key) if r["unblocks"] > 0][:top]
    for r in shown:
        why = " (blocked)" if r["blocked"] else ""
        out.append(f"{r['unblocks']:>8} {r['value']:>6}  {r['id']:<7} {str(r['status']) + why:<12} "
                   f"{'-' if r['depth'] is None else r['depth']:>5}  {_ms(r)[:28]:<28} {r['title'][:50]}")
    if not shown:
        out.append("       -  nothing open holds another open ticket")
    out.append("frontier: " + (" ".join(a["frontier"][:30]) + (" …" if len(a["frontier"]) > 30 else "") or "(empty)"))
    for d, ids in sorted(a["groups"].items(), key=lambda kv: (kv[0] == "cycle", int(kv[0]) if kv[0].isdigit() else 0)):
        label = "ready now / being worked" if d == "0" else ("in a cycle" if d == "cycle" else f"{d} landing(s) away")
        out.append(f"depth {d} ({label}): {len(ids)}" + (f" - {' '.join(ids)}" if show_order else ""))
    return out


def committed_ref(repo: str, ops: str | None = None) -> str:
    """main - or, while a batch gates, the bulk marker's `base=` (main's tip is provisional then)."""
    ops = ops or os.environ.get("HACKRIFF_OPS") or os.path.expanduser("~/.hackriff-ops")
    try:
        for ln in open(os.path.join(ops, "bulk-in-progress"), encoding="utf-8"):
            if ln.startswith("base=") and ln.split("=", 1)[1].strip():
                return ln.split("=", 1)[1].strip()
    except OSError:
        pass
    return "main"


def committed_tasks(repo: str, ops: str | None = None) -> tuple[list[dict], str]:
    """(tasks, ref) from the committed board - never a working tree, never a provisional tip."""
    import yaml
    ref = committed_ref(repo, ops)
    text = subprocess.run(["git", "-C", repo, "show", f"{ref}:docs/tasks.yaml"], capture_output=True,
                          text=True, timeout=30, check=True).stdout
    return (yaml.safe_load(text) or {}).get("tasks") or [], ref
