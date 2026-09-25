"""`just radio` - the HackRF radio lock: ONE owner of the radio at a time (T-922).

WHY. The Mac Studio has one HackRF, and three things want it: the staging demo (`ops/stage.sh`,
live on :8899), the explorer agent (a bounded window driving the live app, T-923) and the
capture-agent (SigMF fixtures). Until now "who has the radio" was answered by trying to open it,
which the staging daemon did every restart - and `hackrf_info` exits 0 even when the open fails.

The lock is `$HACKRIFF_OPS/radio-lock`, a few `key=value` lines:

    owner=explorer
    since=1790300000          (epoch seconds)
    until=1790310800          (epoch seconds; past it the lock is STALE)
    why=explorer window 1: FM/RDS, NOAA WX, APRS

* `take <owner> <duration> <why>` refuses while a live (non-stale) lock is held by anyone -
  including the same owner, so a second window cannot silently extend the first; release first.
  A stale lock is replaced, and the takeover says whose it was.
* `release <owner>` removes the lock only if `owner` holds it (stale or not).
* `status` prints the holder, until when, and the staging mode (`$HACKRIFF_OPS/hk-serve-source`).
* `release-stale` removes a lock past its `until` and prints what it removed (the watchdog does
  this on every tick, with an alert).

`ops/stage.sh` respects the lock: while it is held by anyone other than `stage` the demo runs its
looping SigMF replay, and it goes back to LIVE on the HackRF when the lock is released. Stdlib
only: `ops/watchdog.py` imports this module by path.
"""

from __future__ import annotations

import argparse
import os
import re
import sys
import time

LOCK_NAME = "radio-lock"
#: The staging daemon's own name: a lock it holds does not send it to replay.
STAGE_OWNER = "stage"
_DUR = re.compile(r"(\d+)\s*([hms])")
_UNIT = {"h": 3600, "m": 60, "s": 1}


def ops_dir() -> str:
    return os.environ.get("HACKRIFF_OPS") or os.path.expanduser("~/.hackriff-ops")


def lock_path(ops: str | None = None) -> str:
    return os.path.join(ops or ops_dir(), LOCK_NAME)


def parse_duration(text: str) -> int:
    """`3h`, `90m`, `45s`, `2h30m` -> seconds. A bare number is minutes. Refuses 0 and junk."""
    t = text.strip().lower()
    if t.isdigit():
        secs = int(t) * 60
    else:
        parts = _DUR.findall(t)
        if not parts or _DUR.sub("", t).strip():
            raise ValueError(f"not a duration: {text!r} (use e.g. 3h, 90m, 2h30m)")
        secs = sum(int(n) * _UNIT[u] for n, u in parts)
    if secs <= 0:
        raise ValueError(f"duration must be positive: {text!r}")
    return secs


def read(ops: str | None = None) -> dict | None:
    """The lock as a dict (since/until as ints), or None when there is none. A file that exists but
    will not parse is still a lock - owner `?`, until 0, so it reads as stale and the watchdog
    clears it with an alert rather than every caller treating it as free or as held forever."""
    try:
        text = open(lock_path(ops)).read()
    except FileNotFoundError:
        return None
    d: dict = {}
    for line in text.splitlines():
        k, sep, v = line.partition("=")
        if sep:
            d[k.strip()] = v.strip()
    lock = {"owner": d.get("owner") or "?", "why": d.get("why", "")}
    for k in ("since", "until"):
        try:
            lock[k] = int(float(d.get(k, "0")))
        except ValueError:
            lock[k] = 0
    return lock


def is_stale(lock: dict, now: float | None = None) -> bool:
    return (time.time() if now is None else now) > lock["until"]


def holder(ops: str | None = None, now: float | None = None) -> dict | None:
    """The live (non-stale) lock, or None when the radio is free."""
    lock = read(ops)
    if lock is None or is_stale(lock, now):
        return None
    return lock


def _hhmm(epoch: int) -> str:
    return time.strftime("%Y-%m-%d %H:%M", time.localtime(epoch))


def describe(lock: dict, now: float | None = None) -> str:
    now = time.time() if now is None else now
    left = lock["until"] - now
    tail = "STALE" if left < 0 else f"{int(left // 60)} min left"
    return (f"{lock['owner']} since {_hhmm(lock['since'])} until {_hhmm(lock['until'])} ({tail})"
            + (f" - {lock['why']}" if lock["why"] else ""))


def _write(lock: dict, ops: str | None = None) -> None:
    path = lock_path(ops)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    body = "".join(f"{k}={lock[k]}\n" for k in ("owner", "since", "until", "why"))
    tmp = f"{path}.{os.getpid()}.tmp"
    with open(tmp, "w") as f:
        f.write(body)
    os.replace(tmp, path)


def take(owner: str, seconds: int, why: str, ops: str | None = None,
         now: float | None = None) -> tuple[bool, str]:
    owner, why = owner.strip(), " ".join(why.split())
    if not owner or not re.fullmatch(r"[A-Za-z0-9_.@:-]+", owner):
        return False, f"refused: owner must be one word ([A-Za-z0-9_.@:-]), got {owner!r}"
    if not why:
        return False, "refused: say why you are taking the radio"
    now = time.time() if now is None else now
    old = read(ops)
    if old is not None and not is_stale(old, now):
        return False, f"refused: the radio is held by {describe(old, now)}"
    lock = {"owner": owner, "since": int(now), "until": int(now) + int(seconds), "why": why}
    _write(lock, ops)
    msg = f"taken: {describe(lock, now)}"
    if old is not None:
        msg += f" (replaced a stale lock: {describe(old, now)})"
    return True, msg


def release(owner: str, ops: str | None = None, now: float | None = None) -> tuple[bool, str]:
    lock = read(ops)
    if lock is None:
        return True, "not held - nothing to release"
    if lock["owner"] != owner.strip():
        return False, f"refused: the radio is held by {describe(lock, now)}, not {owner!r}"
    os.remove(lock_path(ops))
    return True, f"released: {lock['owner']}"


def release_stale(ops: str | None = None, now: float | None = None) -> dict | None:
    """Remove a lock past its `until`; return what was removed (None if nothing was stale)."""
    lock = read(ops)
    if lock is None or not is_stale(lock, now):
        return None
    try:
        os.remove(lock_path(ops))
    except FileNotFoundError:
        return None
    return lock


def staging_mode(ops: str | None = None) -> str:
    try:
        return open(os.path.join(ops or ops_dir(), "hk-serve-source")).read().strip().replace("source: ", "")
    except OSError:
        return "unknown (no hk-serve-source)"


def status(ops: str | None = None, now: float | None = None) -> str:
    lock = read(ops)
    who = "free" if lock is None else describe(lock, now)
    return f"radio: {who}\nstaging: {staging_mode(ops)}"


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="just radio", description="the HackRF radio lock ($HACKRIFF_OPS/radio-lock)")
    sub = ap.add_subparsers(dest="cmd", required=True)
    t = sub.add_parser("take", help="take the radio: take <owner> <duration e.g. 3h> <why...>")
    t.add_argument("owner")
    t.add_argument("duration")
    t.add_argument("why", nargs="+")
    r = sub.add_parser("release", help="release your own lock: release <owner>")
    r.add_argument("owner")
    sub.add_parser("status", help="holder, until, and the staging mode")
    sub.add_parser("release-stale", help="remove a lock past its until (the watchdog runs this)")
    a = ap.parse_args(argv)
    if a.cmd == "take":
        try:
            secs = parse_duration(a.duration)
        except ValueError as e:
            print(f"refused: {e}", file=sys.stderr)
            return 2
        ok, msg = take(a.owner, secs, " ".join(a.why))
    elif a.cmd == "release":
        ok, msg = release(a.owner)
    elif a.cmd == "release-stale":
        gone = release_stale()
        ok, msg = True, ("released stale lock: " + describe(gone)) if gone else "no stale lock"
    else:
        ok, msg = True, status()
    print(msg, file=sys.stdout if ok else sys.stderr)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
