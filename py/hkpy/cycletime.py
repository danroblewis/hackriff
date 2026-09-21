"""`just cycle-time` — what the ticket cycle actually costs, from records rather than memory.

T-543 was filed because iteration felt like ~4 h per ticket and nobody could produce a
number. This reads the two places the answer is already written down and prints it:

  * **`$HACKRIFF_OPS/gate-timings.jsonl`** — written by `py/hkpy/gatelog.py` from inside
    `just gate`, so every gate run records its class, its phase, each suite's duration and
    the load average it ran under. That is the *cost* half.
  * **`$HACKRIFF_OPS/merge-runner.log`** — already written by `ops/merge-runner.sh`, which
    has always known when a branch was picked up, when its gate started and whether it
    merged. That is the *queue* half, and it is readable retroactively: the history from
    before this ticket landed is still in the file, so the tool has data on its first run.

Plus git, for when a branch was cut and when its first and last commits landed. Between the
three, a ticket's life reads: **branch cut -> first commit -> last commit -> gate start ->
merged**, and the gap that dominates is visible instead of assumed.

WHY THE MERGE LOG IS PARSED RATHER THAN REPLACED. `ops/merge-runner.sh` is a shell loop that
already emits exactly the lines needed, twice over (its `log()` both `tee`s and inherits a
redirect, so every line appears in duplicate — deduped here). Adding a second, structured
ledger next to a log that already carries the facts would be two sources to keep in step;
the only line added to the runner is the one fact it never recorded, the moment a branch
was first SEEN in the queue, which is where a deep queue spends its time.

A missing file is not an error. A machine that has never run a gate prints a header and
nothing else — the tool says "no data", it does not pretend.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import statistics
import subprocess
import sys
from dataclasses import dataclass, field
from datetime import datetime, timedelta

try:  # `python -m hkpy.cycletime`
    from . import gatelog
except ImportError:  # `python3 py/hkpy/cycletime.py`
    sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    from hkpy import gatelog  # type: ignore[no-redef]

#: `[09-20 15:03:05] GATE task-t531 (just gate-merge; may take 15-25 min)…`
#: The runner's timestamps carry no year — it prints `%m-%d %H:%M:%S` — so a year is
#: supplied from the file's own modification time and rolled back over a December boundary.
_LINE = re.compile(r"^\[(\d\d)-(\d\d) (\d\d):(\d\d):(\d\d)\] (.*)$")

_EVENTS = (
    ("QUEUED", re.compile(r"^QUEUED (\S+)")),
    ("MERGE start", re.compile(r"^MERGE start (\S+)")),
    ("GATE", re.compile(r"^GATE (\S+) \(just gate-merge")),
    ("MERGED", re.compile(r"^MERGED (\S+)")),
    ("GATE FAILED", re.compile(r"^GATE FAILED (\S+)")),
    ("BULK gate", re.compile(r"^BULK gate \(")),
    ("BULK MERGED", re.compile(r"^BULK MERGED . (.*)$")),
)


@dataclass
class Event:
    when: datetime
    kind: str
    branch: str | None


def parse_runner_log(text: str, year: int) -> list[Event]:
    """Every recognised event in `ops/merge-runner.log`, oldest first, deduplicated.

    Duplicates are real and structural: the runner's `log()` writes through `tee` while the
    process's own stdout is redirected to the same file, so every line lands twice. Identical
    (timestamp, kind, branch) triples therefore collapse to one — which also makes the parse
    idempotent if the log is ever concatenated after a restart.
    """
    out: list[Event] = []
    seen: set[tuple[datetime, str, str | None]] = set()
    prev: datetime | None = None
    cur_year = year
    for raw in text.splitlines():
        m = _LINE.match(raw.strip())
        if not m:
            continue
        mon, day, hh, mm, ss, rest = m.groups()
        try:
            when = datetime(cur_year, int(mon), int(day), int(hh), int(mm), int(ss))
        except ValueError:
            continue
        # The log has no year. It is written in order, so a timestamp that jumps far
        # backwards is a December -> January wrap; everything before it belongs to the
        # previous year. Without this, a log spanning New Year sorts into nonsense.
        if prev is not None and when < prev - timedelta(days=200):
            cur_year += 1
            when = when.replace(year=cur_year)
        prev = when
        for kind, pattern in _EVENTS:
            hit = pattern.match(rest)
            if not hit:
                continue
            branch = hit.group(1) if hit.groups() else None
            key = (when, kind, branch)
            if key not in seen:
                seen.add(key)
                out.append(Event(when, kind, branch))
            break
    return out


@dataclass
class BranchLife:
    """One branch's life, as far as the records show it."""

    branch: str
    first_commit: datetime | None = None
    last_commit: datetime | None = None
    queued: datetime | None = None
    gate_start: datetime | None = None
    merged: datetime | None = None
    gate_failures: int = 0
    gates: list[float] = field(default_factory=list)

    @property
    def gate_seconds(self) -> float | None:
        if self.gate_start and self.merged:
            return (self.merged - self.gate_start).total_seconds()
        return None

    @property
    def wait_seconds(self) -> float | None:
        """Last commit -> gate start: the queue, which is where a deep backlog hides."""
        start = self.queued or self.gate_start
        if self.last_commit and start:
            return max(0.0, (start - self.last_commit).total_seconds())
        return None

    @property
    def total_seconds(self) -> float | None:
        """First commit -> merged: the number T-543 is about."""
        if self.first_commit and self.merged:
            return (self.merged - self.first_commit).total_seconds()
        return None


def lives_from_events(events: list[Event]) -> dict[str, BranchLife]:
    lives: dict[str, BranchLife] = {}

    def life(branch: str) -> BranchLife:
        return lives.setdefault(branch, BranchLife(branch=branch))

    for ev in events:
        if ev.branch is None:
            continue
        if ev.kind == "QUEUED":
            entry = life(ev.branch)
            if entry.queued is None:
                entry.queued = ev.when
        elif ev.kind in ("MERGE start", "GATE"):
            entry = life(ev.branch)
            entry.gate_start = ev.when
        elif ev.kind == "MERGED":
            entry = life(ev.branch)
            entry.merged = ev.when
        elif ev.kind == "GATE FAILED":
            entry = life(ev.branch)
            entry.gate_failures += 1
    return lives


def read_landed(ops: str) -> list[dict]:
    """`$HACKRIFF_OPS/landed.jsonl`, one record per landed ticket. Missing file -> []."""
    out: list[dict] = []
    try:
        with open(os.path.join(ops, "landed.jsonl"), encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    rec = json.loads(line)
                except ValueError:
                    continue
                if isinstance(rec, dict):
                    out.append(rec)
    except OSError:
        return []
    return out


def _commit_times(root: str, rev_range: str) -> tuple[datetime | None, datetime | None]:
    proc = subprocess.run(
        ["git", "log", "--format=%ct", rev_range],
        cwd=root,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0 or not proc.stdout.strip():
        return None, None
    stamps = sorted(int(s) for s in proc.stdout.split())
    if not stamps:
        return None, None
    return datetime.fromtimestamp(stamps[0]), datetime.fromtimestamp(stamps[-1])


def git_commit_times(root: str, branch: str) -> tuple[datetime | None, datetime | None]:
    """(first, last) commit time on `branch` since it diverged from main.

    Two sources, in order, because the interesting branches are the ones that are GONE. The
    merge runner deletes a branch's worktree on success and the ref is often pruned with it,
    so asking only `main..<branch>` would report nothing for exactly the merged work whose
    cycle time this tool exists to measure.

      1. `main..<branch>` while the ref still exists;
      2. otherwise the merge commit on main that names the branch, whose **second parent**
         is the branch tip: `<merge>^1..<merge>^2` is the branch's own commits, preserved in
         main's history forever.

    `(None, None)` if neither answers — a branch merged by hand with no recognisable message,
    say. Reported as `-`, never as a guess.
    """
    first, last = _commit_times(root, f"main..{branch}")
    if first is not None:
        return first, last
    merge = subprocess.run(
        [
            "git",
            "log",
            "main",
            "--merges",
            "--format=%H",
            "--grep",
            branch,
            "--fixed-strings",
            "-n",
            "1",
        ],
        cwd=root,
        capture_output=True,
        text=True,
        check=False,
    )
    sha = merge.stdout.strip().splitlines()[0] if merge.stdout.strip() else ""
    if not sha:
        return None, None
    return _commit_times(root, f"{sha}^1..{sha}^2")


def _fmt(seconds: float | None) -> str:
    if seconds is None:
        return "     -"
    if seconds < 90:
        return f"{seconds:5.0f}s"
    return f"{seconds / 60:5.1f}m"


def _pct(values: list[float], q: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    idx = min(len(ordered) - 1, max(0, int(round(q * (len(ordered) - 1)))))
    return ordered[idx]


def report(root: str, limit: int = 20) -> list[str]:
    ops = gatelog.ops_dir()
    out = [f"cycle-time: ops dir = {ops}"]

    # ---- gate durations, per class and per phase ------------------------------------
    records = gatelog.read()
    runs = gatelog.runs(records)
    if not runs:
        out.append(
            "cycle-time: no gate timings yet — "
            f"{gatelog.log_path()} is empty or missing. It fills on the next `just gate`."
        )
    else:
        out.append("")
        out.append(f"GATE RUNS ({len(runs)} recorded, newest last)")
        by_class: dict[str, list[float]] = {}
        by_cmd: dict[str, list[float]] = {}
        unfinished = 0
        for run in runs:
            if not run["finished"]:
                unfinished += 1
                continue
            secs = run.get("seconds")
            if isinstance(secs, (int, float)):
                by_class.setdefault(str(run["class"]), []).append(float(secs))
            for suite in run["suites"]:
                s = suite.get("seconds")
                if isinstance(s, (int, float)):
                    by_cmd.setdefault(str(suite.get("cmd")), []).append(float(s))
        out.append(f"  {'class':14s} {'runs':>5s} {'median':>8s} {'p90':>8s} {'max':>8s}")
        for klass, vals in sorted(by_class.items()):
            out.append(
                f"  {klass:14s} {len(vals):5d} "
                f"{_fmt(statistics.median(vals)):>8s} "
                f"{_fmt(_pct(vals, 0.9)):>8s} {_fmt(max(vals)):>8s}"
            )
        if unfinished:
            out.append(
                f"  {unfinished} run(s) started and never finished — killed, starved or "
                "still going. Those are the ones worth looking at."
            )
        out.append("")
        out.append("  PER PHASE (the suite commands the gate actually launched)")
        for cmd, vals in sorted(by_cmd.items(), key=lambda kv: -statistics.median(kv[1])):
            out.append(
                f"  {cmd:24s} {len(vals):5d} "
                f"{_fmt(statistics.median(vals)):>8s} "
                f"{_fmt(_pct(vals, 0.9)):>8s} {_fmt(max(vals)):>8s}"
            )

    # ---- branch lifecycle, from the merge runner's own log ---------------------------
    runner_log = os.path.join(ops, "merge-runner.log")
    if not os.path.exists(runner_log):
        out.append("")
        out.append(f"cycle-time: no {runner_log} — branch lifecycle unavailable.")
        return out
    try:
        with open(runner_log, encoding="utf-8", errors="replace") as fh:
            text = fh.read()
    except OSError as exc:
        out.append(f"cycle-time: cannot read {runner_log}: {exc}")
        return out

    year = datetime.fromtimestamp(os.path.getmtime(runner_log)).year
    events = parse_runner_log(text, year)
    lives = lives_from_events(events)
    merged = [life for life in lives.values() if life.merged]
    merged.sort(key=lambda entry: entry.merged or datetime.min)
    for life in merged:
        life.first_commit, life.last_commit = git_commit_times(root, life.branch)

    out.append("")
    out.append(f"BRANCHES MERGED ({len(merged)} in {os.path.basename(runner_log)})")
    out.append(
        f"  {'branch':22s} {'commit->merge':>13s} {'queue wait':>11s} "
        f"{'gate':>7s} {'fails':>6s}  merged"
    )
    for life in merged[-limit:]:
        out.append(
            f"  {life.branch:22s} {_fmt(life.total_seconds):>13s} "
            f"{_fmt(life.wait_seconds):>11s} {_fmt(life.gate_seconds):>7s} "
            f"{life.gate_failures:6d}  {life.merged:%m-%d %H:%M}"
        )

    # T-543: the per-ticket ledger `ops/merge-runner.sh` writes on every land. It is the
    # authoritative commit->merge number (the runner reads the merge commit's second parent,
    # which survives the branch being deleted); the table above is the same figure recovered
    # retroactively for tickets that landed before the ledger existed.
    landed = read_landed(ops)
    if landed:
        minutes = [
            float(rec["land_minutes"])
            for rec in landed
            if isinstance(rec.get("land_minutes"), (int, float))
        ]
        attempts = [
            int(rec["gate_attempts"])
            for rec in landed
            if isinstance(rec.get("gate_attempts"), int)
        ]
        out.append("")
        out.append(f"LANDED LEDGER ({len(landed)} ticket(s) in landed.jsonl)")
        for rec in landed[-limit:]:
            out.append(
                f"  {str(rec.get('ticket')):10s} land {str(rec.get('land_minutes')):>6s} min"
                f"  gate attempts {rec.get('gate_attempts')}"
            )
        if minutes:
            out.append(
                f"  first commit -> merged: median {statistics.median(minutes):.0f} min, "
                f"p90 {_pct(minutes, 0.9):.0f} min over {len(minutes)} ticket(s)"
            )
        if attempts:
            out.append(
                f"  gate attempts per landed ticket: median {statistics.median(attempts):.1f}, "
                f"max {max(attempts)}"
            )

    gates = [life.gate_seconds for life in merged if life.gate_seconds]
    waits = [life.wait_seconds for life in merged if life.wait_seconds]
    totals = [life.total_seconds for life in merged if life.total_seconds]
    out.append("")
    out.append("SUMMARY (merge-runner gates only; a gate an agent ran itself is not here)")
    for name, vals in (("gate", gates), ("queue wait", waits), ("commit->merge", totals)):
        if vals:
            out.append(
                f"  {name:14s} n={len(vals):3d}  median {_fmt(statistics.median(vals))}"
                f"  p90 {_fmt(_pct(vals, 0.9))}  max {_fmt(max(vals))}"
            )
        else:
            out.append(f"  {name:14s} no data")
    failed = sum(life.gate_failures for life in lives.values())
    if failed:
        out.append(
            f"  {failed} gate failure(s) recorded — each one costs a full gate and a requeue."
        )
    return out


#: The rolling-median budgets a regression test asserts against (T-543, user 2026-09-20).
#: A slow-down should trip a TEST, not wait for someone to notice. Numbers are the measured
#: 2026-09-20 medians with headroom, not aspirations: gate median was 21.4 min, so `full` is
#: budgeted at 35 min; the cheap classes were seconds.
#:
#: The test is deliberately a MEDIAN over the most recent runs, not a per-run assertion: this
#: box runs up to four agents plus an `hk serve` by policy, and a single contended run is not
#: evidence of anything (a `cargo build -p hk-plugins --bins` documented at 0.05 s was
#: measured at 500 s under load 211). A median that moves is.
BUDGET_S: dict[str, float] = {
    "full": 35 * 60,
    "ui": 8 * 60,
    "ui+docs": 8 * 60,
    "py": 3 * 60,
    "docs": 30,
}

#: How many recent runs of a class the rolling median is taken over, and the minimum number
#: below which the test has nothing to say and must not fail.
ROLLING_WINDOW = 7
MIN_SAMPLES = 5


def rolling_medians(
    runs_: list[dict], window: int = ROLLING_WINDOW
) -> dict[str, tuple[float, int]]:
    """Per class: (median of the last `window` FINISHED runs, how many there were).

    Unfinished runs are excluded — a killed gate has no duration — but they are counted and
    reported elsewhere, never silently treated as fast.
    """
    per: dict[str, list[float]] = {}
    for run in runs_:
        if not run.get("finished"):
            continue
        secs = run.get("seconds")
        klass = run.get("class")
        if isinstance(secs, (int, float)) and isinstance(klass, str):
            per.setdefault(klass, []).append(float(secs))
    return {
        klass: (statistics.median(vals[-window:]), len(vals[-window:]))
        for klass, vals in per.items()
    }


def budget_breaches(
    runs_: list[dict],
    budgets: dict[str, float] | None = None,
    window: int = ROLLING_WINDOW,
    min_samples: int = MIN_SAMPLES,
) -> list[str]:
    """Classes whose rolling median exceeds its budget, as human sentences.

    Empty means nothing to report — including "not enough runs yet", which is the common
    case on a fresh machine and must never read as a pass or a failure.
    """
    budgets = BUDGET_S if budgets is None else budgets
    out: list[str] = []
    for klass, (median, n) in sorted(rolling_medians(runs_, window).items()):
        budget = budgets.get(klass)
        if budget is None or n < min_samples:
            continue
        if median > budget:
            out.append(
                f"{klass}: rolling median of the last {n} runs is {median / 60:.1f} min, "
                f"over the {budget / 60:.1f} min budget"
            )
    return out


def stats(runs_: list[dict]) -> list[str]:
    """p50/p90 per class and per phase — `just gate-stats`."""
    out: list[str] = []
    by_class: dict[str, list[float]] = {}
    by_cmd: dict[str, list[float]] = {}
    unfinished = 0
    for run in runs_:
        if not run.get("finished"):
            unfinished += 1
            continue
        secs = run.get("seconds")
        if isinstance(secs, (int, float)):
            by_class.setdefault(str(run.get("class")), []).append(float(secs))
        for suite in run.get("suites", []):
            sec = suite.get("seconds")
            if isinstance(sec, (int, float)):
                by_cmd.setdefault(str(suite.get("cmd")), []).append(float(sec))
    if not by_class and not unfinished:
        out.append(
            "gate-stats: no runs recorded yet — "
            f"{gatelog.log_path()} fills on the next `just gate`."
        )
        return out
    out.append(f"{'class':14s} {'runs':>5s} {'p50':>8s} {'p90':>8s} {'max':>8s}")
    for klass, vals in sorted(by_class.items()):
        out.append(
            f"{klass:14s} {len(vals):5d} {_fmt(statistics.median(vals)):>8s} "
            f"{_fmt(_pct(vals, 0.9)):>8s} {_fmt(max(vals)):>8s}"
        )
    if unfinished:
        out.append(f"(+{unfinished} run(s) started and never finished — killed or starved)")
    if by_cmd:
        out.append("")
        out.append(f"{'phase':24s} {'runs':>5s} {'p50':>8s} {'p90':>8s} {'max':>8s}")
        for cmd, vals in sorted(by_cmd.items(), key=lambda kv: -statistics.median(kv[1])):
            out.append(
                f"{cmd:24s} {len(vals):5d} {_fmt(statistics.median(vals)):>8s} "
                f"{_fmt(_pct(vals, 0.9)):>8s} {_fmt(max(vals)):>8s}"
            )
    breaches = budget_breaches(runs_)
    out.append("")
    if breaches:
        out.append("OVER BUDGET (py/tests/test_gate.py asserts this too):")
        out.extend(f"  {line}" for line in breaches)
    else:
        out.append("within budget (or not enough runs yet to say)")
    return out


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="just cycle-time",
        description="Gate durations by class and phase, and branch cut -> merged, from records.",
    )
    parser.add_argument("--limit", type=int, default=20, help="branches to list (default 20)")
    parser.add_argument("--root", default=None, help="repo root (default: git toplevel)")
    parser.add_argument(
        "--stats",
        action="store_true",
        help="p50/p90 per class and per phase only — what `just gate-stats` prints",
    )
    args = parser.parse_args(argv)
    if args.stats:
        for line in stats(gatelog.runs(gatelog.read())):
            print(line)
        return 0
    root = args.root
    if root is None:
        proc = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            check=False,
        )
        root = proc.stdout.strip() if proc.returncode == 0 else os.getcwd()
    for line in report(root, limit=args.limit):
        print(line)
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
