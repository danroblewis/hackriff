"""Cheap branches first, alone (user, 2026-09-24).

"Give dashboard-only branches queue priority: if the runner is mid-batch and a py+ops or ui-only
branch is queued, it should be the very next attempt, alone, so it lands in ~2 min after the batch
rather than joining the next full one." A branch whose diff classifies as anything but `full`
(hkpy.gate.classify - the same rule the gate itself applies) is CHEAP; when the queue holds cheap
and full branches, ops/merge-runner.sh gates the cheap ones as the next attempt by themselves and
puts the rest back in order. Nothing about any gate changes: the cheap attempt is classified and
gated by `just gate` exactly as before, and a full branch still gets the full gate.

    python -m hkpy.gatepri <base> <branch>...   ->   "cheap: a b" / "rest: c d"
"""

from __future__ import annotations

import subprocess
import sys

from hkpy import gate


def branch_class(repo: str, base: str, branch: str) -> str:
    """The gate class of what `branch` would put on `base`; `full` when git cannot say."""
    try:
        out = subprocess.run(["git", "-C", repo, "diff", "--name-only", "--no-renames", "-z", f"{base}...{branch}"],
                             capture_output=True, text=True, timeout=60, check=True).stdout
    except Exception:
        return gate.FULL
    return gate.classify([p for p in out.split("\0") if p]).label   # --no-renames: a move out of crates/ is full


def partition(classes: dict[str, str], order: list[str]) -> tuple[list[str], list[str]]:
    """(cheap, rest) in queue order. Pure."""
    cheap = [b for b in order if classes.get(b, gate.FULL) != gate.FULL]
    return cheap, [b for b in order if b not in cheap]


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) < 2:
        print("usage: python -m hkpy.gatepri <base> <branch>...", file=sys.stderr)
        return 2
    base, branches = argv[0], argv[1:]
    repo = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True).stdout.strip() or "."
    classes = {b: branch_class(repo, base, b) for b in branches}
    cheap, rest = partition(classes, branches)
    print("cheap: " + " ".join(cheap))
    print("rest: " + " ".join(rest))
    print("classes: " + " ".join(f"{b}={classes[b]}" for b in branches))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
