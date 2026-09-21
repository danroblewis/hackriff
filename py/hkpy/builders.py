"""`just builders` — whether it is safe to launch another Rust-building agent (T-559).

CLAUDE.md's "Worktree launch step" caps concurrent Rust-building agents at **4** on the
dev Mac's 28 cores. That cap was a number in a coordinator's head — and on 2026-09-20 it
missed the `hk serve` processes agents leave running behind it (e2e harnesses, demo
servers, replay servers): T-543 measured a build documented at 0.05 s warm taking 500 s at
load average 211, with the top consumer an `hk serve` at 930-1300 % CPU that a
count-the-cargo-processes check could not see. The cap does not bound what it exists to
bound unless something actually counts what is running, the same move T-396 made for the
gate decision and T-477 made for the board: put the check in a command, not in a
coordinator's memory.

THE COUNTING RULE, stated explicitly because it is the whole point of the ticket:

  * An `hk`/`hackriffd` server process (`hk serve`, `hk replay --serve`, a browser-tier
    `hk serve` under test, a live-radio session, ...) counts toward the cap **on its own**,
    whether or not anything is compiling in its worktree. Missing this is exactly the T-543
    defect: an idle-looking worktree can still be holding ten cores.
  * `cargo`/`rustc` processes are grouped **by worktree**: `CARGO_BUILD_JOBS=6` means one
    building agent is six-plus OS processes (cargo plus its rustc children), and that must
    still cost exactly one slot, not six — otherwise the cap would trip on a single agent's
    own parallelism.
  * The coordinator's own full gate, run from the main checkout (not a worktree), is one
    more source of cargo/rustc processes and is grouped the same way — into the worktree
    label `"main"` — so it costs exactly one slot, per CLAUDE.md's "the coordinator's full
    check counts as one".
  * A worktree that is *both* building *and* running a server is charged for both: compute
    and a resident process/port are different resources, and suppressing either count would
    silently reintroduce the failure this tool exists to close.

Process classification and the cap arithmetic are a **pure function**, `assess()`, of an
already-gathered process table plus the load/disk readings — same shape as `hkpy.gate`'s
`classify()`: it takes data in, never shells out, so it is testable with synthetic process
tables (`py/tests/test_builders.py`). `main()` is the only part that touches the OS: it
gathers `ps` output (and, best-effort, each candidate's cwd via `lsof`) and hands it to
`assess()`.

THIS TOOL ONLY REPORTS. It must never kill, renice or otherwise touch a process — the
user's live demo and other agents' harnesses are running processes this tool will see and
must leave alone.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass, field

#: CLAUDE.md's "Worktree launch step": at most 4 Rust-building agents at once (the
#: coordinator's own full check counts as one).
CAP = 4

#: CLAUDE.md: "don't launch below ~20 GB free" (checked via `df -h /`).
DISK_FLOOR_GB = 20.0

#: The dev Mac has 28 cores (CLAUDE.md, T-144). A 1-minute load average past the box's own
#: core count means more runnable work exists than the box can run at once *before* anyone
#: launches anything else — the T-543 incident measured 129-211. Overridable per call
#: (`--cores`) for a different machine; never guessed from `os.cpu_count()` inside the pure
#: function, so a test can pin it.
DEFAULT_CORES = 28

#: `.claude/worktrees/<name>` is this repo's one worktree layout (see CLAUDE.md's
#: "Coordination" and every ticket's own worktree-setup step).
_WORKTREE_RE = re.compile(r"\.claude/worktrees/([^/\s]+)")

_CARGO_RUSTC_EXES = {"cargo", "rustc"}

#: Both binaries this workspace ships that serve a port: `hk` (hk-cli's `serve`/`replay
#: --serve`) and `hackriffd` (the replay daemon, hk-cli/src/bin/hackriffd.rs).
_HK_EXES = {"hk", "hackriffd"}

#: `cargo run -p <package> --bin <name> -- ...`: the package name when no explicit `--bin`
#: is given. Best-effort — good enough to catch the common invocations without needing to
#: parse Cargo.toml.
_PACKAGE_TO_BIN = {"hk-cli": "hk", "hackriffd": "hackriffd"}

_BIND_RE = re.compile(r"--bind[= ]+(\S+)")
#: `1,5` digits, not `2,5` — the workspace's own tests bind `127.0.0.1:0` (ephemeral port),
#: and a single-digit port must still parse rather than falling back to the whole address.
_PORT_RE = re.compile(r":(\d{1,5})\b")


@dataclass(frozen=True)
class ProcInfo:
    """One running process, as much as `main()` could learn about it.

    `cwd` is best-effort (via `lsof` on the dev Mac) and may be `None` — `worktree_of`
    falls back to scanning `command` for the same `.claude/worktrees/<name>` pattern, so a
    missing cwd degrades the *label* only, never drops the process from the count.
    """

    pid: int
    command: str
    cwd: str | None = None


@dataclass(frozen=True)
class ServerProc:
    """One `hk`/`hackriffd` server-class process — counts toward the cap on its own."""

    pid: int
    worktree: str
    subcommand: str | None
    port: str | None
    command: str


@dataclass(frozen=True)
class Assessment:
    """The build-pressure picture and the verdict `assess()` reached from it."""

    cargo_by_worktree: dict = field(default_factory=dict)
    servers: tuple = ()
    builder_count: int = 0
    cap: int = CAP
    loadavg1: float = 0.0
    cores: int = DEFAULT_CORES
    disk_free_gb: float = 0.0
    disk_floor_gb: float = DISK_FLOOR_GB
    safe: bool = True
    reasons: tuple = ()
    verdict: str = ""


def _exe_name(command: str) -> str:
    stripped = command.strip()
    if not stripped:
        return ""
    return stripped.split(None, 1)[0].rsplit("/", 1)[-1]


def _tokens(command: str) -> list[str]:
    return command.split()


def _cargo_run_bin(toks: list[str]) -> str | None:
    """The binary a `cargo run ...` invocation executes, if this is one."""
    for i, t in enumerate(toks):
        if t == "--bin" and i + 1 < len(toks):
            return toks[i + 1]
    for i, t in enumerate(toks):
        if t in ("-p", "--package") and i + 1 < len(toks):
            return _PACKAGE_TO_BIN.get(toks[i + 1])
    return None


def classify_process(command: str) -> str | None:
    """`'hk_server'`, `'cargo_build'`, or `None` for anything else. Each process gets exactly

    one bucket: a `cargo run -p hk-cli --bin hk -- serve ...` invocation is `hk_server`, not
    also `cargo_build` — what it is *doing* is running the served binary (compiling it first
    if needed), not compiling on the cap's behalf, and counting one process against both
    buckets would double-charge it. Priority is `hk_server` first for exactly that reason.
    """
    exe = _exe_name(command)
    toks = _tokens(command)
    if exe in _HK_EXES:
        return "hk_server"
    if exe == "cargo" and len(toks) > 1 and toks[1] == "run":
        if _cargo_run_bin(toks) in _HK_EXES:
            return "hk_server"
    if exe in _CARGO_RUSTC_EXES:
        return "cargo_build"
    return None


def _hk_args(command: str) -> list[str]:
    toks = _tokens(command)
    if "--" in toks:
        return toks[toks.index("--") + 1 :]
    if _exe_name(command) in _HK_EXES:
        return toks[1:]
    return []


def hk_subcommand(command: str) -> str | None:
    """The first non-flag argument after the `hk`/`hackriffd` invocation, for display."""
    for t in _hk_args(command):
        if not t.startswith("-"):
            return t
    return None


def server_port(command: str) -> str | None:
    """The bind port from `--bind ADDR`, if present. `None` for a process with no bind flag

    (e.g. a plain `hk replay` with no `--serve`, which this classifier still counts as an
    `hk_server`-class process because it still holds a live decode pipeline's cores).
    """
    m = _BIND_RE.search(command)
    if not m:
        return None
    addr = m.group(1)
    pm = _PORT_RE.search(addr)
    return pm.group(1) if pm else addr


def worktree_of(command: str, cwd: str | None) -> str:
    """Which worktree a process belongs to, for grouping.

    Checks `cwd` first, then falls back to scanning `command` itself (a build's argv often
    carries an absolute path even when the cwd could not be read). Anything recognized as a
    cargo/rustc/hk process but naming no `.claude/worktrees/<name>` path is assumed to be
    the coordinator's own main checkout (`"main"`) — the common case this ticket's own
    acceptance measurement was about — never dropped or mislabeled `"unknown"` in a way that
    would let it escape the count; the label only affects the printed grouping; see
    `assess()` for why it can't affect the cap arithmetic in a way that matters.
    """
    for candidate in (cwd, command):
        if not candidate:
            continue
        m = _WORKTREE_RE.search(candidate)
        if m:
            return f"worktree:{m.group(1)}"
    return "main"


def assess(
    processes,
    *,
    loadavg1: float,
    disk_free_gb: float,
    cap: int = CAP,
    disk_floor_gb: float = DISK_FLOOR_GB,
    cores: int = DEFAULT_CORES,
) -> Assessment:
    """The build-pressure picture and verdict. Pure: no git, no `ps`, no side effects.

    `processes` is the full, already-classified-nothing process table (any `ProcInfo`s are
    fine, including ones this function will ignore) — `assess` does its own filtering via
    `classify_process`, so a caller never has to pre-filter, and a test can hand it a
    synthetic table including unrelated system processes to prove they're ignored.
    """
    cargo_by_worktree: dict[str, int] = {}
    servers: list[ServerProc] = []
    for p in processes:
        kind = classify_process(p.command)
        if kind is None:
            continue
        wt = worktree_of(p.command, p.cwd)
        if kind == "cargo_build":
            cargo_by_worktree[wt] = cargo_by_worktree.get(wt, 0) + 1
        else:
            servers.append(
                ServerProc(
                    pid=p.pid,
                    worktree=wt,
                    subcommand=hk_subcommand(p.command),
                    port=server_port(p.command),
                    command=p.command,
                )
            )

    builder_count = len(cargo_by_worktree) + len(servers)

    reasons: list[str] = []
    if disk_free_gb < disk_floor_gb:
        reasons.append(
            f"disk free {disk_free_gb:.1f} GB is below the {disk_floor_gb:.0f} GB floor"
        )
    if loadavg1 > cores:
        reasons.append(
            f"1-min load {loadavg1:.1f} exceeds the {cores}-core budget"
        )
    if builder_count >= cap:
        reasons.append(
            f"{builder_count} builder(s) already running "
            f"({len(cargo_by_worktree)} cargo/rustc worktree(s) + {len(servers)} hk "
            f"serve/run process(es)) at or over the cap of {cap}"
        )

    safe = not reasons
    if safe:
        verdict = f"safe to launch another builder ({builder_count}/{cap} slots in use)"
    else:
        verdict = f"NOT safe to launch another builder ({builder_count}/{cap} slots in use): " + "; ".join(
            reasons
        )

    return Assessment(
        cargo_by_worktree=cargo_by_worktree,
        servers=tuple(servers),
        builder_count=builder_count,
        cap=cap,
        loadavg1=loadavg1,
        cores=cores,
        disk_free_gb=disk_free_gb,
        disk_floor_gb=disk_floor_gb,
        safe=safe,
        reasons=tuple(reasons),
        verdict=verdict,
    )


def render(a: Assessment) -> list[str]:
    """`Assessment` as printed lines: the picture, then the verdict — never the verdict alone,

    same reasoning as `hkpy.gate.render`: a one-line answer with no evidence behind it is not
    checkable by the person reading it.
    """
    out: list[str] = []
    if a.cargo_by_worktree:
        out.append("builders: cargo/rustc, by worktree:")
        for wt in sorted(a.cargo_by_worktree):
            out.append(f"builders:   {wt}  ({a.cargo_by_worktree[wt]} process(es))")
    else:
        out.append("builders: cargo/rustc: none running")

    if a.servers:
        out.append("builders: hk serve/run processes:")
        for s in sorted(a.servers, key=lambda s: (s.worktree, s.pid)):
            port = s.port or "no --bind seen"
            sub = s.subcommand or "?"
            out.append(
                f"builders:   pid {s.pid}  {s.worktree}  `{sub}`  port={port}"
            )
    else:
        out.append("builders: hk serve/run processes: none running")

    out.append(
        f"builders: load(1m)  = {a.loadavg1:.2f}  (budget: {a.cores} cores)"
    )
    out.append(
        f"builders: disk free = {a.disk_free_gb:.1f} GB  (floor: {a.disk_floor_gb:.0f} GB)"
    )
    out.append(
        f"builders: slots     = {a.builder_count}/{a.cap} "
        f"({len(a.cargo_by_worktree)} cargo worktree(s) + {len(a.servers)} server(s))"
    )
    out.append(f"builders: verdict   = {a.verdict}")
    return out


# ---------------------------------------------------------------------------
# Gathering the real process table (main() only — everything above is pure)
# ---------------------------------------------------------------------------


def _ps_lines() -> list[str]:
    """`pid command` for every process, one per line. `[]` if `ps` is unavailable."""
    try:
        proc = subprocess.run(
            ["ps", "-axo", "pid=,command="],
            capture_output=True,
            text=True,
            check=False,
            timeout=10,
        )
    except (OSError, subprocess.TimeoutExpired):
        return []
    if proc.returncode != 0:
        return []
    return proc.stdout.splitlines()


def _cwd_via_lsof(pid: int) -> str | None:
    """Best-effort cwd for one pid via `lsof` (macOS `ps` has no cwd column). `None` on any

    failure — a missing cwd only degrades `worktree_of`'s label, never drops the process.
    """
    try:
        proc = subprocess.run(
            ["lsof", "-a", "-p", str(pid), "-d", "cwd", "-Fn"],
            capture_output=True,
            text=True,
            check=False,
            timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if proc.returncode != 0:
        return None
    for line in proc.stdout.splitlines():
        if line.startswith("n"):
            return line[1:] or None
    return None


def gather_processes() -> list[ProcInfo]:
    """The real process table, filtered to what `classify_process` would look at, with

    best-effort cwds. Filtering here (not in `assess`) keeps `lsof` calls to the handful of
    candidate pids instead of every process on the box.
    """
    out: list[ProcInfo] = []
    for line in _ps_lines():
        line = line.strip()
        if not line or " " not in line:
            continue
        pid_s, command = line.split(None, 1)
        try:
            pid = int(pid_s)
        except ValueError:
            continue
        if classify_process(command) is None:
            continue
        out.append(ProcInfo(pid=pid, command=command, cwd=_cwd_via_lsof(pid)))
    return out


def _loadavg1() -> float:
    try:
        return os.getloadavg()[0]
    except (OSError, AttributeError):  # pragma: no cover - not all platforms
        return 0.0


def _disk_free_gb(path: str = "/") -> float:
    try:
        return shutil.disk_usage(path).free / (1024**3)
    except OSError:  # pragma: no cover - path always exists in practice
        return 0.0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="just builders",
        description=(
            "Print the current build-pressure picture (cargo/rustc by worktree, hk "
            "serve/run processes, load, disk) and whether it is safe to launch another "
            "Rust-building agent. Read-only: never kills or touches a process."
        ),
    )
    parser.add_argument("--cap", type=int, default=CAP, help=f"default: {CAP}")
    parser.add_argument(
        "--disk-floor-gb", type=float, default=DISK_FLOOR_GB, help=f"default: {DISK_FLOOR_GB:g}"
    )
    parser.add_argument("--cores", type=int, default=DEFAULT_CORES, help=f"default: {DEFAULT_CORES}")
    parser.add_argument(
        "--strict",
        action="store_true",
        help="exit 1 when it is not safe to launch another builder",
    )
    args = parser.parse_args(argv)

    processes = gather_processes()
    a = assess(
        processes,
        loadavg1=_loadavg1(),
        disk_free_gb=_disk_free_gb(),
        cap=args.cap,
        disk_floor_gb=args.disk_floor_gb,
        cores=args.cores,
    )
    for line in render(a):
        print(line, flush=True)
    return 1 if (args.strict and not a.safe) else 0


if __name__ == "__main__":
    sys.exit(main())
