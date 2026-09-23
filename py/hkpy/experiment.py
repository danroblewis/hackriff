"""`just experiment` — the pipeline experiment ledger, one open at a time.

WHY. A knob changed without a baseline, a metric and a rollback is a guess that cannot be told
from noise a day later. The ledger (`docs/ops-experiments.md` for people, `$HACKRIFF_OPS/
experiments.jsonl` for the tools) makes every pipeline change a claim with a before, an after and
a decision, and the code enforces the rules a person would forget under pressure: `new` refuses
while one is open and requires a rollback line; `close` refuses `keep` when a guard is broken and
charges the blocked minutes (holds and WORK_CAP=1 periods) against the result.

Metrics are `hkpy.flow.summary()` fields. Guards are one-line expressions:
    <metric> <= baseline*<factor>     e.g. "real_reds_24h <= baseline*1.25"
    <metric> <  <number>              e.g. "blocked_minutes < 30"
    <metric> >= <number>              e.g. "landings_per_h_6h >= 1.7"
Metric names: landings_per_h_6h, landings_per_h_24h, real_reds_24h, flakes_24h, reds_24h,
full_gate_p50_min, conflicts_24h, touchpoints_24h, hours_with_dispatch_24h, blocked_minutes.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from datetime import datetime

from hkpy import flow

OPS = os.environ.get("HACKRIFF_OPS") or os.path.expanduser("~/.hackriff-ops")
LEDGER_MD = "docs/ops-experiments.md"
_GUARD = re.compile(r"^\s*([a-z_0-9]+)\s*(<=|>=|<|>)\s*(baseline\s*\*\s*([0-9.]+)|[0-9.]+)\s*$")


def _root(root: str | None) -> str:
    if root:
        return root
    try:
        return subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip()
    except Exception:
        return os.getcwd()


def ledger_path(ops: str) -> str:
    return os.path.join(ops, "experiments.jsonl")


def records(ops: str) -> list[dict]:
    out = []
    try:
        with open(ledger_path(ops), encoding="utf-8") as fh:
            for line in fh:
                try:
                    out.append(json.loads(line))
                except ValueError:
                    continue
    except FileNotFoundError:
        pass
    return out


def current(ops: str) -> dict | None:
    """The open experiment's `open` record, or None."""
    opened: dict | None = None
    for r in records(ops):
        if r.get("event") == "open":
            opened = r
        elif r.get("event") == "close" and opened and r.get("id") == opened.get("id"):
            opened = None
    return opened


def _append(ops: str, rec: dict) -> None:
    os.makedirs(ops, exist_ok=True)
    with open(ledger_path(ops), "a", encoding="utf-8") as fh:
        fh.write(json.dumps(rec) + "\n")


def parse_guard(text: str) -> dict:
    m = _GUARD.match(text)
    if not m:
        raise ValueError(f"guard not understood: {text!r} (form: <metric> <= baseline*1.25 | <metric> < 30)")
    metric, op, rhs, factor = m.groups()
    return {"metric": metric, "op": op, "factor": float(factor) if factor else None,
            "value": None if factor else float(rhs), "text": text.strip()}


def _window(text: str, now: datetime) -> tuple[datetime, datetime]:
    """'2026-09-23 00:00..13:00' | '2026-09-23T00:00..2026-09-23T13:00' | '24h' (ending now)."""
    if ".." in text:
        a, b = text.split("..", 1)
        a = a.strip().replace(" ", "T")
        b = b.strip().replace(" ", "T")
        t0 = datetime.fromisoformat(a)
        t1 = datetime.fromisoformat(b if "T" in b else f"{a.split('T')[0]}T{b}")
        return t0, t1
    return flow._parse_since(text, now), now


def metrics_over(ops: str, t0: datetime, t1: datetime) -> dict:
    """flow summary fields recomputed over an arbitrary window (not just the trailing 6/24 h)."""
    hours = max(1e-9, (t1 - t0).total_seconds() / 3600)
    h = flow.hourly(ops, t0, t1)
    g = [r for r in flow.gate_rows(ops, t0, t1) if r["verdict"] in ("green", "red")]
    full = sorted(r["minutes"] for r in g if r["class"] == "full" and r["minutes"])
    reds = [r for r in g if r["verdict"] == "red"]
    return {
        "landings_per_h_6h": round(sum(r["landed"] for r in h) / hours, 2),   # over the window
        "landings_per_h_24h": round(sum(r["landed"] for r in h) / hours, 2),
        "dispatch": sum(r["dispatch"] for r in h), "handback": sum(r["handback"] for r in h),
        "hours_with_dispatch_24h": sum(1 for r in h if r["dispatch"] > 0),
        "gates": len(g), "reds_24h": len(reds),
        "real_reds_24h": sum(1 for r in reds if r["cause"] in ("real", "flake-then-real")),
        "flakes_24h": sum(1 for r in g if r["cause"] in ("flake", "flake-then-real")),
        "full_gate_p50_min": (full[len(full) // 2] if full else None),
        "conflicts_24h": sum(r["conflicts"] for r in flow.gate_rows(ops, t0, t1)),
        "touchpoints_24h": len(flow.touchpoints(ops, t0, t1)),
        "blocked_minutes": blocked_minutes(ops, t0, t1),
        "window": f"{t0.strftime('%Y-%m-%d %H:%M')}..{t1.strftime('%Y-%m-%d %H:%M')}", "hours": round(hours, 1),
    }


def blocked_minutes(ops: str, t0: datetime, t1: datetime) -> float:
    """Minutes the pipeline was held (hold.jsonl) or dispatch floored (env.jsonl WORK_CAP=1) in the window."""
    total = 0.0
    # A hold is charged from its start to the FIRST end event after it - `release` (just hold
    # --release), `expired` or `ended-by-queue` (the runner) - and only to `until` when no end was
    # recorded. Charging every hold its full window billed a 30-minute hold the runner ended in
    # 40 seconds as 30 blocked minutes (review, 2026-09-23), which invariant 7 turns into a verdict.
    holds = flow._jsonl(os.path.join(ops, "hold.jsonl"))
    for i, r in enumerate(holds):
        if r.get("event") != "hold":
            continue
        a = datetime.fromtimestamp(float(r.get("ts", 0)))
        b = datetime.fromtimestamp(float(r.get("until", 0)))
        for later in holds[i + 1:]:
            if later.get("event") in ("release", "expired", "ended-by-queue"):
                b = min(b, datetime.fromtimestamp(float(later.get("ts", 0))))
                break
        total += flow._overlap_minutes(a, b, t0, t1)
    floor_since: datetime | None = None
    for r in flow._jsonl(os.path.join(ops, "env.jsonl")):
        t = datetime.fromtimestamp(float(r.get("ts", 0)))
        cap = (r.get("set") or {}).get("WORK_CAP")
        if cap == "1" and floor_since is None:
            floor_since = t
        elif floor_since is not None and (cap not in (None, "1") or "WORK_CAP" in (r.get("unset") or {}) or "reset" in r):
            total += flow._overlap_minutes(floor_since, t, t0, t1)
            floor_since = None
    if floor_since is not None:
        total += flow._overlap_minutes(floor_since, t1, t0, t1)
    return round(total, 1)


def evaluate(guard: dict, now_val: float | None, base_val: float | None) -> tuple[bool | None, str]:
    """(ok, text). ok=None when a value is missing."""
    if now_val is None:
        return None, f"{guard['metric']}: no data yet"
    bound = guard["value"] if guard["factor"] is None else (None if base_val is None else base_val * guard["factor"])
    if bound is None:
        return None, f"{guard['metric']}: baseline missing"
    ok = {"<=": now_val <= bound, "<": now_val < bound, ">=": now_val >= bound, ">": now_val > bound}[guard["op"]]
    return ok, f"{guard['metric']} {now_val} {guard['op']} {round(bound, 2)} {'ok' if ok else 'BROKEN'}"


def cmd_new(ops: str, root: str, a: argparse.Namespace, now: datetime | None = None) -> int:
    now = now or datetime.now()
    if current(ops):
        print(f"experiment: {current(ops)['id']} is open - close it first (invariant 12: one at a time)", file=sys.stderr)
        return 3
    if not a.rollback.strip():
        print("experiment: --rollback is required (invariant 15: no rollback line, no experiment)", file=sys.stderr)
        return 2
    try:
        guards = [parse_guard(g) for g in a.guard]
    except ValueError as e:
        print(f"experiment: {e}", file=sys.stderr)
        return 2
    t0, t1 = _window(a.baseline, now)
    baseline = metrics_over(ops, t0, t1)
    rec = {"event": "open", "id": a.id, "ts": now.timestamp(), "opened": now.strftime("%Y-%m-%d %H:%M"),
           "hypothesis": a.hypothesis, "knobs": a.knob, "baseline_window": baseline["window"], "baseline": baseline,
           "metric": a.metric, "guards": guards, "gates": a.gates, "hours": a.hours, "rule": a.rule,
           "rollback": a.rollback, "owner": os.environ.get("HACKRIFF_ROLE") or "pipeline"}
    _append(ops, rec)
    md = os.path.join(root, LEDGER_MD)
    block = (f"\n---\n\n## {a.id} — {a.hypothesis[:80]}\n\n"
             f"- **Opened:** {rec['opened']} · **owner:** {rec['owner']}\n"
             f"- **Hypothesis:** {a.hypothesis}\n- **Knob:** {', '.join(a.knob)}\n"
             f"- **Baseline window:** {baseline['window']} — landings/h {baseline['landings_per_h_24h']}, "
             f"real reds {baseline['real_reds_24h']}/{baseline['gates']}, full-gate p50 {baseline['full_gate_p50_min']} min, "
             f"dispatch-hours {baseline['hours_with_dispatch_24h']}, blocked {baseline['blocked_minutes']} min\n"
             f"- **Primary metric:** {a.metric}\n- **Guards:** {'; '.join(g['text'] for g in guards)}\n"
             f"- **Duration:** {a.gates} gates or {a.hours} h\n- **Decision rule:** {a.rule}\n"
             f"- **Rollback:** `{a.rollback}`\n- **Status:** open\n- **Result:** —\n")
    try:
        with open(md, "a", encoding="utf-8") as fh:
            fh.write(block)
    except OSError as e:
        print(f"experiment: ledger jsonl written; could not append to {md}: {e}", file=sys.stderr)
    print(f"experiment: {a.id} opened; baseline {baseline['window']}: landings/h {baseline['landings_per_h_24h']}, "
          f"real reds {baseline['real_reds_24h']}/{baseline['gates']}, p50 full gate {baseline['full_gate_p50_min']} min")
    return 0


def status_of(ops: str, now: datetime | None = None) -> tuple[dict | None, dict | None, list[tuple[bool | None, str]]]:
    now = now or datetime.now()
    cur = current(ops)
    if not cur:
        return None, None, []
    t0 = datetime.fromtimestamp(cur["ts"])
    m = metrics_over(ops, t0, now)
    checks = [evaluate(g, m.get(g["metric"]), cur["baseline"].get(g["metric"])) for g in cur["guards"]]
    return cur, m, checks


def cmd_status(ops: str) -> int:
    cur, m, checks = status_of(ops)
    if not cur:
        print("experiment: none open")
        return 0
    metric = cur["metric"].split()[0]
    base = cur["baseline"].get(metric)
    val = m.get(metric)
    delta = "" if base in (None, 0) or val is None else f" ({(val - base) / base * 100:+.0f}% vs baseline {base})"
    print(f"{cur['id']}: {cur['hypothesis'][:90]}")
    print(f"  open since {cur['opened']} · {m['hours']} h · {m['gates']}/{cur['gates']} gates counted · knobs {', '.join(cur['knobs'])}")
    print(f"  {metric}: {val}{delta}")
    for ok, text in checks:
        print(f"  guard {text}")
    print(f"  blocked minutes: {m['blocked_minutes']} · rule: {cur['rule']} · rollback: {cur['rollback']}")
    return 0


def cmd_close(ops: str, root: str, decision: str, note: str, now: datetime | None = None) -> int:
    now = now or datetime.now()
    cur, m, checks = status_of(ops, now)
    if not cur:
        print("experiment: none open", file=sys.stderr)
        return 3
    broken = [t for ok, t in checks if ok is False]
    if decision == "keep" and broken:
        print("experiment: refusing 'keep' - a guard is broken (invariant 14): " + "; ".join(broken), file=sys.stderr)
        return 3
    rec = {"event": "close", "id": cur["id"], "ts": now.timestamp(), "closed": now.strftime("%Y-%m-%d %H:%M"),
           "decision": decision, "note": note, "result": m, "guards": [t for _, t in checks], "blocked_minutes": m["blocked_minutes"]}
    _append(ops, rec)
    md = os.path.join(root, LEDGER_MD)
    metric = cur["metric"].split()[0]
    line = (f"\n**{cur['id']} closed {rec['closed']} — {decision.upper()}.** {metric}: {cur['baseline'].get(metric)} → {m.get(metric)} "
            f"over {m['gates']} gates / {m['hours']} h; real reds {m['real_reds_24h']}/{m['gates']}; full-gate p50 {m['full_gate_p50_min']} min; "
            f"blocked {m['blocked_minutes']} min; guards: {'; '.join(t for _, t in checks) or 'none'}. {note}\n")
    try:
        with open(md, "a", encoding="utf-8") as fh:
            fh.write(line)
    except OSError:
        pass
    print(line.strip())
    return 0


def cmd_list(ops: str) -> int:
    opened: dict[str, dict] = {}
    for r in records(ops):
        if r.get("event") == "open":
            opened[r["id"]] = {"opened": r["opened"], "hyp": r["hypothesis"][:70], "decision": "open"}
        elif r.get("event") == "close" and r.get("id") in opened:
            opened[r["id"]]["decision"] = f"{r['decision']} {r['closed']}"
    for k, v in opened.items():
        print(f"{k:<8}{v['opened']:<18}{v['decision']:<28}{v['hyp']}")
    if not opened:
        print("experiment: ledger empty")
    return 0


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="just experiment", description=__doc__.split("\n\n")[0])
    p.add_argument("--ops", default=OPS)
    p.add_argument("--root", default=None)
    sub = p.add_subparsers(dest="cmd", required=True)
    n = sub.add_parser("new")
    n.add_argument("--id", required=True)
    n.add_argument("--hypothesis", required=True)
    n.add_argument("--knob", action="append", required=True)
    n.add_argument("--baseline", required=True)
    n.add_argument("--metric", required=True)
    n.add_argument("--guard", action="append", default=[])
    n.add_argument("--gates", type=int, default=6)
    n.add_argument("--hours", type=float, default=8)
    n.add_argument("--rule", required=True)
    n.add_argument("--rollback", required=True)
    sub.add_parser("status")
    c = sub.add_parser("close")
    c.add_argument("--decision", choices=["keep", "rollback", "inconclusive"], required=True)
    c.add_argument("--note", default="")
    sub.add_parser("list")
    a = p.parse_args(argv)
    root = _root(a.root)
    if a.cmd == "new":
        return cmd_new(a.ops, root, a)
    if a.cmd == "status":
        return cmd_status(a.ops)
    if a.cmd == "close":
        return cmd_close(a.ops, root, a.decision, a.note)
    if a.cmd == "list":
        return cmd_list(a.ops)
    return 2


if __name__ == "__main__":
    sys.exit(main())
