"""Reap an explorer window's IQ rings, and detect a rogue `hk serve` the window did not start
(T-983).

WHY. The explorer agent (`.claude/agents/explorer.md`, T-923) is told to run ONE `hk serve` for
its whole window and retune it between targets. On 2026-09-25 it didn't: window 2 ran one `hk
serve` per target, each preallocating its own ~4.2 GB IQ ring under a fresh `--data-dir`, and
nothing reaped them unless the agent remembered to on its own way out (window 1 left one at
7.9 GB). A later amendment found the same thing happening even after the rule was stated at
launch, so `ops/explorer-window.sh` now starts the window's one server itself and polls for a
second one to shut down, rather than trusting the agent's compliance.

This module is the pure, testable half of that: given a window directory, find the IQ ring
directories under it (`ring_dirs`/`find_data_dirs`/`plan_reap`/`reap`); given a `ps` process
table, find `hk serve` processes whose `--data-dir` sits under the window but are not the one
server the window started (`parse_process_table`/`find_rogue`). `ops/explorer-window.sh` calls
this file's CLI (`reap` / `rogue` subcommands) rather than reimplementing either in bash.

The ring layout matches `hk-pipeline`'s `iqbuffer.rs`: a single-device server puts its ring at
`<data_dir>/iqbuffer`; a multi-device server puts each device's ring under
`<data_dir>/iqbuffer-devices/<device>`. Nothing else under a data dir (history, recordings,
*.db, logs) is a ring, and this module never touches it. Stdlib only, like `hkpy.radio`.
"""

from __future__ import annotations

import argparse
import shlex
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

RING_DIRNAMES = ("iqbuffer", "iqbuffer-devices")


def ring_dirs(data_dir: Path) -> list[Path]:
    """The IQ ring directories directly under one `hk serve --data-dir` — never `history/`,
    `recordings/`, a `*.db` or logs beside them."""
    data_dir = Path(data_dir)
    return [data_dir / name for name in RING_DIRNAMES if (data_dir / name).is_dir()]


def _dir_size(path: Path) -> int:
    total = 0
    for p in Path(path).rglob("*"):
        try:
            if p.is_file() and not p.is_symlink():
                total += p.stat().st_size
        except OSError:
            pass
    return total


def find_data_dirs(window_dir: Path, exclude: Path | None = None) -> list[Path]:
    """Every directory under `window_dir` that looks like an `hk serve --data-dir` (it has a ring
    subdirectory), including `window_dir` itself. Stops descending once a data dir is found — a
    rogue server's `--data-dir` is a sibling under the window tree, never nested inside another
    server's own data dir — so this never walks a live ring's own file tree looking for more.

    `exclude`, if given, is never returned and is never descended into: a rogue's reported
    `--data-dir` is a process's own claim, not something to trust blindly, and a rogue that shares
    the kept server's data dir (or names a parent of it) must never cause a walk into the kept
    server's still-live ring (T-983 fix round 2 - `reap`'s caller passes the kept server's own
    data dir here when reaping anything found under a *rogue's* claimed dir)."""
    window_dir = Path(window_dir)
    if not window_dir.is_dir():
        return []
    excl = exclude.resolve() if exclude is not None else None
    found: list[Path] = []

    def walk(d: Path) -> None:
        if excl is not None and d.resolve() == excl:
            return
        if ring_dirs(d):
            found.append(d)
            return
        try:
            children = [c for c in d.iterdir() if c.is_dir() and not c.is_symlink()]
        except OSError:
            return
        for c in children:
            walk(c)

    walk(window_dir)
    return found


def plan_reap(window_dir: Path, exclude: Path | None = None) -> list[tuple[Path, int]]:
    """`(ring_dir, bytes)` for every ring directory under `window_dir` — what `reap` would
    delete. Used for the dry-run print and for the real reap's own accounting. `exclude`: see
    `find_data_dirs`."""
    out: list[tuple[Path, int]] = []
    for dd in find_data_dirs(window_dir, exclude=exclude):
        for rd in ring_dirs(dd):
            out.append((rd, _dir_size(rd)))
    return out


def reap(window_dir: Path, dry_run: bool = False, exclude: Path | None = None) -> tuple[list[tuple[Path, int]], int]:
    """Delete every ring directory under `window_dir` (unless `dry_run`) and return the plan plus
    the total bytes reclaimed. Only entries named in `RING_DIRNAMES` are ever removed; history,
    the journal, captures and server.* records are untouched because they are never a ring dir.

    `exclude` (T-983 fix round 2): a data dir that is never reaped and never walked into, however
    it's reached — equal to `window_dir` itself, nested under it, or an ancestor a rogue's claimed
    `--data-dir` walks down through. The watcher passes the window's KEPT server's data dir here
    whenever it reaps a rogue's claimed dir, so a rogue that shares (or contains) the kept dir can
    never take the kept server's still-live ring down with it - only ever a genuine, separate one."""
    import shutil

    plan = plan_reap(window_dir, exclude=exclude)
    total = sum(b for _, b in plan)
    if not dry_run:
        for rd, _ in plan:
            shutil.rmtree(rd, ignore_errors=True)
    return plan, total


@dataclass(frozen=True)
class ServerProc:
    pid: int
    data_dir: Path
    args: str


def parse_data_dir(args: str) -> str | None:
    """`--data-dir VALUE` or `--data-dir=VALUE` out of a command line; None if absent."""
    try:
        parts = shlex.split(args)
    except ValueError:
        parts = args.split()
    for i, a in enumerate(parts):
        if a == "--data-dir" and i + 1 < len(parts):
            return parts[i + 1]
        if a.startswith("--data-dir="):
            return a.split("=", 1)[1]
    return None


def parse_process_table(lines: list[str]) -> list[ServerProc]:
    """Parse `ps -eo pid=,args=` (Linux) / `ps -axo pid=,args=` (macOS) output lines into the
    `hk serve` processes among them — anything else, or an `hk serve` with no `--data-dir`, is
    skipped (nothing to compare against the window)."""
    out: list[ServerProc] = []
    for line in lines:
        line = line.strip()
        if not line:
            continue
        pid_s, _, args = line.partition(" ")
        args = args.strip()
        try:
            pid = int(pid_s)
        except ValueError:
            continue
        if "hk" not in args or "serve" not in args:
            continue
        dd = parse_data_dir(args)
        if dd is None:
            continue
        out.append(ServerProc(pid=pid, data_dir=Path(dd), args=args))
    return out


def find_rogue(processes: list[ServerProc], window_dir: Path, keep_pid: int | None) -> list[ServerProc]:
    """Every `hk serve` process whose `--data-dir` sits under `window_dir` but is not
    `keep_pid` — a second server the agent started per-target despite the rule (a new
    `--data-dir`), or a second listener started after the first (any pid, even the same
    `--data-dir`, that isn't the one the window recorded)."""
    window_dir = Path(window_dir).resolve()
    out: list[ServerProc] = []
    for p in processes:
        try:
            dd = p.data_dir.resolve()
        except OSError:
            dd = p.data_dir
        under = dd == window_dir or window_dir in dd.parents
        if under and (keep_pid is None or p.pid != keep_pid):
            out.append(p)
    return out


def _cmd_reap(args: argparse.Namespace) -> int:
    exclude = Path(args.exclude) if args.exclude else None
    plan, total = reap(Path(args.window), dry_run=args.dry_run, exclude=exclude)
    for rd, b in plan:
        print(f"{rd}\t{b}")
    print(f"TOTAL\t{total}")
    return 0


def _cmd_rogue(args: argparse.Namespace) -> int:
    if args.ps_output:
        lines = Path(args.ps_output).read_text().splitlines()
    else:
        out = subprocess.run(["ps", "-eo", "pid=,args="], capture_output=True, text=True, check=False)
        lines = out.stdout.splitlines()
    procs = parse_process_table(lines)
    rogue = find_rogue(procs, Path(args.window), args.keep_pid)
    for p in rogue:
        print(f"{p.pid}\t{p.data_dir}")
    return 0


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)

    r = sub.add_parser("reap", help="delete (or --dry-run print) every ring dir under --window")
    r.add_argument("--window", required=True)
    r.add_argument("--dry-run", action="store_true")
    r.add_argument("--exclude", default=None, help="a data dir never reaped or walked into (the kept server's)")
    r.set_defaults(func=_cmd_reap)

    g = sub.add_parser("rogue", help="print pid<TAB>data-dir for every hk serve under --window that isn't --keep-pid")
    g.add_argument("--window", required=True)
    g.add_argument("--keep-pid", type=int, default=None)
    g.add_argument("--ps-output", default=None, help="a file of ps lines (tests); default runs `ps -eo pid=,args=`")
    g.set_defaults(func=_cmd_rogue)

    ns = ap.parse_args(argv)
    return ns.func(ns)


if __name__ == "__main__":
    sys.exit(main())
