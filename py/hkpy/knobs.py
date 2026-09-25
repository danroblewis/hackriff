"""`just knobs` and `just hold` — the pipeline's runtime knobs and the bounded merge-queue hold.

WHY. Every runner knob is an environment variable read at start, which made an experiment live
only in one process's environment: on 2026-09-23 the cap-6 / pause-12 trial would have silently
reverted on the next plain restart. The store `$HACKRIFF_OPS/env` (KEY=VALUE lines) is what
`ops/launch.sh`, `/dev-env restart` and the runners themselves read on start, and `env.jsonl`
records who changed what, when, and under which experiment. `show` reports the EFFECTIVE value
from the running script's own start line, never the assumed one.

`hold` writes `$HACKRIFF_OPS/hold` (until=, why=, owner=, since=), which `ops/merge-runner.sh`
honours: it takes no branch while the marker is live, ignores it once expired, and ends it at the
first queued branch. The limits are the pipeline-manager invariants 3-6 (`.claude/rules/
pipeline-invariants.md`): at most HOLD_MAX_MIN minutes, no second hold within HOLD_GAP_MIN, never
while a branch is queued or a gate is running, and every hold alerts.
"""

from __future__ import annotations

import argparse
import getpass
import json
import os
import re
import subprocess
import sys
import time

OPS = os.environ.get("HACKRIFF_OPS") or os.path.expanduser("~/.hackriff-ops")
REPO = os.environ.get("HACKRIFF_REPO") or "/Users/daniellewis/hackriff"
HOLD_MAX_MIN = 30
HOLD_GAP_MIN = 120

#: name -> (default as the script computes it, reader, one line). Defaults quoted from the scripts.
KNOBS: dict[str, tuple[str, str, str]] = {
    "WORK_CAP": ("(28-14)/3 = 4", "work-runner", "concurrent workers; a running gate lowers it to the reserve cap in overlap mode"),
    "WORK_QUEUE_PAUSE": ("6", "work-runner", "alone mode only: stop dispatch once this many branches are queued"),
    "WORK_GATE_ALONE": ("0", "work-runner", "1 = the 2026-09-22 cycle (no dispatch while a gate runs or is wanted)"),
    "WORK_PER_TICK": ("2", "work-runner", "dispatches per tick"),
    "WORK_GATE_RESERVE": ("14", "work-runner", "cores kept for the gate; sets the reserve cap"),
    "WORK_WORKER_CORES": ("3", "work-runner", "cores one worker may occupy at peak"),
    "WORK_GROUP_CAP": ("2", "work-runner", "workers per parallel_group"),
    "WORK_MAX_MINUTES": ("180", "work-runner", "a worker older than this is reaped"),
    "WORK_CLONE_TARGET": ("1", "work-runner", "1 = a new worker's worktree clones main's target/; 0 = it builds from sccache (no new clone pin)"),
    "WORKER_DRAIN_MAX": ("0", "merge-runner", "0 = overlap mode; seconds to wait for claimed workers before gating"),
    "FOREIGN_DRAIN_MAX": ("300", "merge-runner", "seconds to wait for a foreign spec run / contention"),
    "BULK_MAX": ("15", "merge-runner", "most branches in one batch"),
    "GATE_TIMEOUT": ("3600", "merge-runner", "seconds before a gate is killed"),
    "MAX_ATTEMPTS": ("2", "merge-runner", "gate attempts per branch tip"),
    "FLAKE_SOLO_ONE": ("0", "merge-runner", "1 = a red the flake ledger already shows passing alone (>=2, never failing, 7 d) is accepted after ONE solo pass"),
    "HK_E2E_CONCURRENCY": ("3", "gate (ui/e2e/run.mjs)", "browser-spec lanes"),
    "NEXTEST_TEST_THREADS": ("profile default 8", "gate (nextest)", "test threads for the workspace suite"),
    "CARGO_BUILD_JOBS": ("6 (gate.py sets it)", "gate only", "parallel rustc jobs for the gate; workers keep their own bound (CARGO_ENV in work-runner.py)"),
}

#: Every knob is a non-negative integer; these must be at least 1 (WORK_CAP=1 is invariant 3's floor).
_AT_LEAST_ONE = {"WORK_CAP", "WORK_PER_TICK", "WORK_WORKER_CORES", "WORK_GROUP_CAP", "WORK_MAX_MINUTES",
                 "BULK_MAX", "GATE_TIMEOUT", "MAX_ATTEMPTS", "HK_E2E_CONCURRENCY", "NEXTEST_TEST_THREADS", "CARGO_BUILD_JOBS"}

_KV = re.compile(r"^([A-Z][A-Z0-9_]+)=(.*)$")


def env_path(ops: str = OPS) -> str:
    return os.path.join(ops, "env")


def read_store(ops: str = OPS) -> dict[str, str]:
    out: dict[str, str] = {}
    try:
        with open(env_path(ops), encoding="utf-8") as fh:
            for line in fh:
                m = _KV.match(line.strip())
                if m:
                    out[m.group(1)] = m.group(2)
    except FileNotFoundError:
        pass
    return out


def write_store(values: dict[str, str], ops: str = OPS) -> None:
    os.makedirs(ops, exist_ok=True)
    tmp = env_path(ops) + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        fh.write("# pipeline knob store - written by `just knobs`; every ops restart sources this file\n")
        for k in sorted(values):
            fh.write(f"{k}={values[k]}\n")
    os.replace(tmp, env_path(ops))


def _append(ops: str, name: str, rec: dict) -> None:
    os.makedirs(ops, exist_ok=True)
    with open(os.path.join(ops, name), "a", encoding="utf-8") as fh:
        fh.write(json.dumps(rec) + "\n")


def open_experiment(ops: str = OPS) -> str | None:
    """The id of the open experiment, from experiments.jsonl, or None."""
    open_id = None
    try:
        with open(os.path.join(ops, "experiments.jsonl"), encoding="utf-8") as fh:
            for line in fh:
                try:
                    r = json.loads(line)
                except ValueError:
                    continue
                if r.get("event") == "open":
                    open_id = r.get("id")
                elif r.get("event") == "close" and r.get("id") == open_id:
                    open_id = None
    except FileNotFoundError:
        pass
    return open_id


def effective(ops: str = OPS) -> dict[str, str]:
    """What the RUNNING scripts say about themselves: the last `KNOBS:`/`VERSION:` start lines."""
    out: dict[str, str] = {}
    for log in ("work-runner.log", "merge-runner.log"):
        try:
            with open(os.path.join(ops, log), "rb") as fh:       # the runner log is never rotated: read only its tail
                fh.seek(0, os.SEEK_END)
                fh.seek(max(0, fh.tell() - 512 * 1024))
                tail = fh.read().decode("utf-8", errors="replace").splitlines()
        except FileNotFoundError:
            continue
        for line in reversed(tail):
            if "KNOBS:" in line:
                for m in re.finditer(r"([A-Z][A-Z0-9_]+)=(\S+)", line.split("KNOBS:", 1)[1]):
                    out.setdefault(m.group(1), m.group(2))
                break
            if log == "work-runner.log" and "VERSION:" in line:
                m = re.search(r"cap=(\d+)", line)
                if m:
                    out.setdefault("WORK_CAP", m.group(1))
    return out


def cmd_show(ops: str) -> int:
    store, eff = read_store(ops), effective(ops)
    print(f"{'knob':<22}{'default':<20}{'stored':<12}{'effective':<12}reader / meaning")
    for k, (d, reader, meaning) in KNOBS.items():
        print(f"{k:<22}{d:<20}{store.get(k, '-'):<12}{eff.get(k, '?'):<12}{reader}: {meaning}")
    extra = sorted(set(store) - set(KNOBS))
    if extra:
        print("stored but unknown to this table:", ", ".join(f"{k}={store[k]}" for k in extra))
    exp = open_experiment(ops)
    print(f"open experiment: {exp or 'none'}   store: {env_path(ops)}")
    return 0


def cmd_set(ops: str, pairs: list[str], why: str, who: str) -> int:
    changes: dict[str, str] = {}
    for p in pairs:
        m = _KV.match(p)
        if not m:
            print(f"knobs: not KEY=VALUE: {p!r}", file=sys.stderr)
            return 2
        k, v = m.group(1), m.group(2)
        if k not in KNOBS:
            print(f"knobs: unknown knob {k}; known: {', '.join(KNOBS)}", file=sys.stderr)
            return 2
        v = v.strip()
        # A stored value the reader cannot parse kills the work runner at its next start (an
        # uncaught ValueError at import) and makes the merge runner print "integer expression
        # expected" every loop - so the store only ever holds what both readers accept.
        if not v.isdigit():
            print(f"knobs: {k}={v!r} is not a non-negative integer; nothing stored", file=sys.stderr)
            return 2
        if k == "WORK_CAP" and int(v) < 1:
            print("knobs: refusing WORK_CAP=0 - dispatch is never fully paused for measurement (invariant 3); "
                  "the floor is 1, and the full stop is $HACKRIFF_OPS/dispatch-paused (user/supervisor only)", file=sys.stderr)
            return 3
        if k in _AT_LEAST_ONE and int(v) < 1:
            print(f"knobs: {k} must be at least 1; nothing stored", file=sys.stderr)
            return 2
        changes[k] = v
    store = read_store(ops)
    before = {k: store.get(k) for k in changes}
    store.update(changes)
    write_store(store, ops)
    exp = open_experiment(ops)
    _append(ops, "env.jsonl", {"ts": time.time(), "who": who, "set": changes, "before": before,
                               "why": why, "experiment": exp, "kind": "experiment" if exp else "incident"})
    tag = f"under {exp}" if exp else "INCIDENT CHANGE (no open experiment)"
    print(f"knobs: stored {' '.join(f'{k}={v}' for k, v in changes.items())} - {tag}; restart the reader to apply "
          f"({', '.join(sorted({KNOBS[k][1] for k in changes}))})")
    return 0


def cmd_unset(ops: str, keys: list[str], who: str) -> int:
    store = read_store(ops)
    gone = {k: store.pop(k) for k in keys if k in store}
    write_store(store, ops)
    _append(ops, "env.jsonl", {"ts": time.time(), "who": who, "unset": gone, "experiment": open_experiment(ops)})
    print(f"knobs: unset {', '.join(gone) or '(nothing was stored)'}; defaults apply on next start")
    return 0


def cmd_reset(ops: str, who: str) -> int:
    store = read_store(ops)
    write_store({}, ops)
    _append(ops, "env.jsonl", {"ts": time.time(), "who": who, "reset": store, "experiment": open_experiment(ops)})
    print(f"knobs: store emptied ({len(store)} knob(s) removed)")
    return 0


# ---- hold --------------------------------------------------------------------------------------

def hold_path(ops: str = OPS) -> str:
    return os.path.join(ops, "hold")


def read_hold(ops: str = OPS) -> dict | None:
    try:
        with open(hold_path(ops), encoding="utf-8") as fh:
            rec = dict(m.groups() for m in (re.match(r"^(\w+)=(.*)$", ln.strip()) for ln in fh) if m)
    except FileNotFoundError:
        return None
    return rec or None


def queue_depth(ops: str = OPS) -> int:
    try:
        with open(os.path.join(ops, "merge-queue.txt"), encoding="utf-8") as fh:
            return sum(1 for ln in fh if ln.strip() and not ln.lstrip().startswith("#"))
    except FileNotFoundError:
        return 0


def gate_running(ops: str = OPS, repo: str = REPO) -> bool:
    return os.path.exists(os.path.join(ops, "bulk-in-progress")) or os.path.exists(os.path.join(repo, ".git", "MERGE_HEAD"))


def last_hold_start(ops: str = OPS) -> float | None:
    last = None
    try:
        with open(os.path.join(ops, "hold.jsonl"), encoding="utf-8") as fh:
            for line in fh:
                try:
                    r = json.loads(line)
                except ValueError:
                    continue
                if r.get("event") == "hold":
                    last = float(r.get("ts", 0))
    except FileNotFoundError:
        pass
    return last


def alert(level: str, title: str, body: str, key: str, repo: str = REPO) -> None:
    try:
        subprocess.run([sys.executable, os.path.join(repo, "ops", "alert.py"), level, title, body, "--key", key],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=20, check=False)
    except Exception:
        pass


def cmd_hold(ops: str, minutes: int, why: str, owner: str, now: float | None = None, repo: str = REPO,
             do_alert: bool = True) -> int:
    now = time.time() if now is None else now
    why = " ".join(why.split())           # one line: the marker is line-oriented and the alert is one sentence
    if minutes <= 0 or minutes > HOLD_MAX_MIN:
        print(f"hold: refusing {minutes} min - the limit is {HOLD_MAX_MIN} (invariant 4); longer needs the user, via the supervisor", file=sys.stderr)
        return 3
    if not why:
        print("hold: --why is required (it is what the alert and the ledger say)", file=sys.stderr)
        return 2
    live = read_hold(ops)
    if live and float(live.get("until", 0) or 0) <= now:
        os.remove(hold_path(ops))          # the runner was down when it expired; an expired marker holds nothing
        live = None
    if live:
        print("hold: one is already in force (just hold --status)", file=sys.stderr)
        return 3
    last = last_hold_start(ops)
    if last is not None and now - last < HOLD_GAP_MIN * 60:
        print(f"hold: refusing - the last hold started {int((now - last) / 60)} min ago; the gap is {HOLD_GAP_MIN} min (invariant 4)", file=sys.stderr)
        return 3
    if queue_depth(ops) > 0:
        print(f"hold: refusing - {queue_depth(ops)} branch(es) are queued; a hold never keeps work waiting (invariant 5)", file=sys.stderr)
        return 3
    if gate_running(ops, repo):
        print("hold: refusing - a gate is running or a merge is staged; wait for it to land", file=sys.stderr)
        return 3
    until = now + minutes * 60
    os.makedirs(ops, exist_ok=True)
    with open(hold_path(ops), "w", encoding="utf-8") as fh:
        fh.write(f"until={int(until)}\nsince={int(now)}\nowner={owner}\nwhy={why}\n")
    _append(ops, "hold.jsonl", {"ts": now, "event": "hold", "minutes": minutes, "until": until, "owner": owner,
                                "why": why, "experiment": open_experiment(ops)})
    when = time.strftime("%H:%M", time.localtime(until))
    print(f"hold: merge queue held until {when} ({minutes} min) - {why}. It ends earlier at the first queued branch.")
    if do_alert:
        alert("amber", "merge queue held", f"{owner}: {why} - until {when} ({minutes} min); ends at the first queued branch.", key=f"hold:{owner}")
    return 0


def cmd_release(ops: str, owner: str, now: float | None = None) -> int:
    now = time.time() if now is None else now
    h = read_hold(ops)
    if not h:
        print("hold: none in force")
        return 0
    os.remove(hold_path(ops))
    held = (now - float(h.get("since", now))) / 60
    _append(ops, "hold.jsonl", {"ts": now, "event": "release", "owner": owner, "held_minutes": round(held, 1), "why": h.get("why")})
    print(f"hold: released after {held:.0f} min")
    return 0


def cmd_hold_status(ops: str, now: float | None = None) -> int:
    now = time.time() if now is None else now
    h = read_hold(ops)
    if not h:
        print("hold: none in force")
        return 0
    left = (float(h.get("until", 0)) - now) / 60
    state = "EXPIRED (the runner ignores it)" if left <= 0 else f"{left:.0f} min left"
    print(f"hold: {h.get('owner')} since {time.strftime('%H:%M', time.localtime(float(h.get('since', 0))))}: {h.get('why')} - {state}")
    return 0


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="just knobs", description=__doc__.split("\n\n")[0])
    p.add_argument("--ops", default=OPS)
    p.add_argument("--who", default=os.environ.get("HACKRIFF_ROLE") or getpass.getuser())
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("show")
    s = sub.add_parser("set")
    s.add_argument("pairs", nargs="+")
    s.add_argument("--why", default="")
    u = sub.add_parser("unset")
    u.add_argument("keys", nargs="+")
    sub.add_parser("reset")
    h = sub.add_parser("hold")
    h.add_argument("--minutes", type=int, default=0)
    h.add_argument("--why", default="")
    h.add_argument("--release", action="store_true")
    h.add_argument("--status", action="store_true")
    h.add_argument("--no-alert", action="store_true")
    a = p.parse_args(argv)
    if a.cmd == "show":
        return cmd_show(a.ops)
    if a.cmd == "set":
        return cmd_set(a.ops, a.pairs, a.why, a.who)
    if a.cmd == "unset":
        return cmd_unset(a.ops, a.keys, a.who)
    if a.cmd == "reset":
        return cmd_reset(a.ops, a.who)
    if a.cmd == "hold":
        if a.release:
            return cmd_release(a.ops, a.who)
        if a.status or not a.minutes:
            return cmd_hold_status(a.ops)
        return cmd_hold(a.ops, a.minutes, a.why, a.who, do_alert=not a.no_alert)
    return 2


if __name__ == "__main__":
    sys.exit(main())
