"""Durable cycle-time instrumentation for the merge gate (T-543).

The gate got slow and nobody could say by how much, when it started, or which half of it
was to blame — the evidence was "the user noticed", plus a wall-clock subtraction of two
lines in `ops/merge-runner.log`. That is a measurement that only exists while someone is
looking at it. This module makes the gate record itself, every run, whoever ran it.

WHAT IS RECORDED, and why each field earns its place:

  * ``class`` and ``phase`` — the gate's whole design is that a `ui` diff costs less than a
    `full` one (T-396). Nobody has ever checked whether it does. Recording the class next
    to the duration turns that from a design intention into a measurable claim, and makes
    "the gate is slow" answerable as "the `full` class is slow, the others are fine".
  * ``suite`` lines, one per command — a gate is `lint` + `test` + `acceptance-ci` +
    `test-ui-e2e`, and 23 minutes attributed to "the gate" says nothing about which of the
    four to attack. Per-phase is the resolution at which a decision can be made.
  * a ``start`` line written BEFORE anything runs, not one line at the end. A gate that is
    killed, starved or abandoned writes no end line, and that is precisely the run worth
    knowing about — the 62-minute starved gate of 2026-09-20 would have left no trace at
    all under an on-completion-only design. An unterminated run is data, so the format has
    to be able to represent one.
  * ``loadavg`` at start — this machine runs up to four building agents plus the gate by
    policy (CLAUDE.md, T-144). A 23-minute gate on an idle box and a 23-minute gate at load
    40 are different facts, and without the load reading they are indistinguishable later.

WHERE IT IS WRITTEN. ``$HACKRIFF_OPS`` (default ``~/.hackriff-ops``), the same directory
`ops/merge-runner.sh` and `ops/monitor.py` already share, deliberately outside `/tmp` so a
reboot cannot wipe it — the ops README's own reasoning, applied to one more file. One
JSON object per line, append-only: concurrent gates in different worktrees interleave
safely, because a single ``write()`` of a short line is atomic on the platforms this runs
on, and nothing ever rewrites an earlier line.

WHAT IT MUST NEVER DO IS FAIL THE GATE. Instrumentation that can break the thing it
measures is worse than none, so every call is wrapped: an unwritable directory, a full
disk or a read-only filesystem degrades to recording nothing, silently. The gate's job is
to gate.
"""

from __future__ import annotations

import json
import os
import socket
import time
import uuid
from typing import Any

#: One JSON object per line, under $HACKRIFF_OPS.
FILENAME = "gate-timings.jsonl"


def ops_dir() -> str:
    """The shared ops state directory — `$HACKRIFF_OPS`, else `~/.hackriff-ops`.

    Same default and same override as `ops/merge-runner.sh`, `ops/stage.sh` and
    `ops/monitor.py`. Kept in one function so the four agree by construction rather than by
    four copies of the same `${HACKRIFF_OPS:-$HOME/.hackriff-ops}`.
    """
    env = os.environ.get("HACKRIFF_OPS", "").strip()
    if env:
        return env
    return os.path.join(os.path.expanduser("~"), ".hackriff-ops")


def log_path() -> str:
    return os.path.join(ops_dir(), FILENAME)


def loadavg() -> list[float] | None:
    try:
        return [round(x, 2) for x in os.getloadavg()]
    except (OSError, AttributeError):  # pragma: no cover - not all platforms
        return None


def append(record: dict[str, Any], path: str | None = None) -> bool:
    """Append one record as a JSON line. Returns True if it was written.

    Never raises. See the module docstring: a gate must not fail because its stopwatch
    could not write.
    """
    try:
        target = path or log_path()
        os.makedirs(os.path.dirname(target), exist_ok=True)
        line = json.dumps(record, separators=(",", ":"), default=str) + "\n"
        with open(target, "a+", encoding="utf-8") as fh:
            # A process killed mid-`write` leaves a partial line with no newline. Without
            # this, the NEXT record would be glued onto that fragment and both would be
            # lost — turning "one run is unreadable" into "one run plus the run after it".
            # So: if the file does not end in a newline, start one.
            fh.seek(0, os.SEEK_END)
            if fh.tell():
                fh.seek(fh.tell() - 1)
                if fh.read(1) != "\n":
                    line = "\n" + line
            fh.write(line)
        return True
    except Exception:
        return False


def new_run_id() -> str:
    """A short id pairing a run's `start`, its `suite` lines and its `end`.

    Random rather than sequential: several gates run concurrently across worktrees (an
    agent's own check while the merge runner gates main), so a counter would collide and a
    pid could be reused after a reboot.
    """
    return uuid.uuid4().hex[:12]


def start_record(
    run_id: str,
    *,
    klass: str,
    phase: str,
    source: str,
    n_files: int,
    crates: list[str] | None = None,
    crate_selection: str | None = None,
    branch: str | None = None,
    sha: str | None = None,
    root: str | None = None,
) -> dict[str, Any]:
    """The line written before any suite runs."""
    return {
        "kind": "gate_start",
        "run": run_id,
        "ts": time.time(),
        "sha": sha,
        "class": klass,
        "phase": phase,
        "source": source,
        "files": n_files,
        "crates": crates,
        "crate_selection": crate_selection,
        "branch": branch,
        "root": root,
        "host": socket.gethostname(),
        "loadavg": loadavg(),
        "pid": os.getpid(),
    }


def suite_record(
    run_id: str, *, cmd: list[str], seconds: float, rc: int
) -> dict[str, Any]:
    """One line per suite command — the per-phase resolution the module exists for."""
    return {
        "kind": "suite",
        "run": run_id,
        "ts": time.time(),
        "cmd": " ".join(cmd),
        "seconds": round(seconds, 1),
        "rc": rc,
        "loadavg": loadavg(),
    }


def end_record(
    run_id: str, *, klass: str, phase: str, seconds: float, rc: int
) -> dict[str, Any]:
    """The closing line. Its absence means the run did not finish — see the docstring."""
    return {
        "kind": "gate_end",
        "run": run_id,
        "ts": time.time(),
        "class": klass,
        "phase": phase,
        "seconds": round(seconds, 1),
        "rc": rc,
        "result": "pass" if rc == 0 else "fail",
        "loadavg": loadavg(),
    }


def read(path: str | None = None) -> list[dict[str, Any]]:
    """Every record in the log, oldest first. Unreadable or malformed lines are skipped.

    A half-written line (a gate killed mid-`write`) must not make the whole history
    unreadable, which is the other half of "an unterminated run is data".
    """
    target = path or log_path()
    out: list[dict[str, Any]] = []
    try:
        with open(target, encoding="utf-8") as fh:
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


def runs(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Fold the flat records into one entry per gate run, in start order.

    A run with a `gate_start` and no `gate_end` comes back with ``finished=False`` and
    ``seconds=None``: killed, starved or still going. That is reported, never dropped.
    """
    by_run: dict[str, dict[str, Any]] = {}
    order: list[str] = []
    for rec in records:
        rid = rec.get("run")
        if not isinstance(rid, str):
            continue
        kind = rec.get("kind")
        if kind == "gate_start":
            if rid not in by_run:
                order.append(rid)
            by_run[rid] = {
                "run": rid,
                "start": rec.get("ts"),
                "class": rec.get("class"),
                "phase": rec.get("phase"),
                "source": rec.get("source"),
                "files": rec.get("files"),
                "crates": rec.get("crates"),
                "crate_selection": rec.get("crate_selection"),
                "branch": rec.get("branch"),
                "loadavg": rec.get("loadavg"),
                "suites": [],
                "finished": False,
                "seconds": None,
                "result": None,
            }
        elif kind == "suite" and rid in by_run:
            by_run[rid]["suites"].append(
                {
                    "cmd": rec.get("cmd"),
                    "seconds": rec.get("seconds"),
                    "rc": rec.get("rc"),
                }
            )
        elif kind == "gate_end" and rid in by_run:
            by_run[rid]["finished"] = True
            by_run[rid]["seconds"] = rec.get("seconds")
            by_run[rid]["result"] = rec.get("result")
            by_run[rid]["end"] = rec.get("ts")
    return [by_run[r] for r in order]
