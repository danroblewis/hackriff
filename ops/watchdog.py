#!/usr/bin/env python3
"""ops/watchdog.py - who is actually using this box, and is anything running that nobody owns?

WHY THIS EXISTS (2026-09-22). A deflaker agent exited and left SIXTEEN
`/bin/zsh -c source …/shell-snapshots/snapshot-zsh-….sh …` busy loops behind, reparented to
launchd (ppid 1), each pinned at 100 % CPU, from 14:29 to 16:47. They ran through every merge
gate in those two and a quarter hours. In the same window a killed merge runner left an orphan
`just gate` running beside its replacement, and `ops/monitor.py` sat at 440 % CPU. None of it
appeared in any log, any dashboard or any alert: it was found because a person typed `ps`.

The budget model in ops/README.md (gate 14 cores, up to four workers x 3 under cpulimit, the
role session 8) is only a budget if something checks it. Every one of those three failures is
invisible to the runners, because each runner knows only about its own children. This script is
the one process whose job is the whole box: every 20 s it builds a process table, attributes
each process to an OWNER (a worker's process group, the merge runner and its gate, a role
session, the demo, the dashboard, the runners themselves) and calls everything else UNOWNED.

ATTRIBUTION FAILS OPEN TO "UNOWNED", NOT TO A NAMED OWNER. The same principle as
`BiasTee::Unknown` is not `Off` and `Coverage::Unobserved` is not quiet: a process we cannot
explain is reported as unexplained, because the whole point is that the sixteen busy loops
belonged to nobody. UNOWNED is not by itself an alarm - a browser, an editor, Spotlight all land
there - so the rules are about sustained CPU, never about mere presence.

IT KILLS ALMOST NOTHING. Two rules kill, each only for the shape of an incident. (b) an unowned
`shell-snapshots/snapshot-zsh` loop (a dead agent's shell; a live agent's shell is a descendant of
its session and is therefore owned) burning >50 % for >10 minutes: SIGKILL. (h) a WORKTREE ORPHAN,
since 2026-09-25, when a `cargo build | grep | head` wrapper in worktrees/t901 ran 15 h with ppid 1
and no claim, and the nextest runs of the sessions killed at 09:34 kept going in four worktrees -
none hot enough for rule (a) even to alarm: an unowned cargo / cargo-nextest / non-sccache rustc /
`hk serve` / ui/e2e node / worktree target binary (or a shell wrapping one) whose worktree, from
its command line or its cwd, has no running or fix-held claim on any host and no owned process in
it, for >10 minutes: SIGTERM, then SIGKILL on a later tick, re-checked against a fresh ps and
claims file before each signal. Everything else is an alert. A watchdog that kills on a guess is worse than the contention it
is watching for, and the alert path (ops/alert.py, deduped 30 min per key) is enough to get a
person or a coordinator to look.

    HACKRIFF_OPS=~/.hackriff-ops nohup python3 ops/watchdog.py >/dev/null 2>&1 & disown
    python3 ops/watchdog.py --once --print      # one tick to stdout, no alerts, no kills
    cat $HACKRIFF_OPS/watchdog.json             # the tick the dashboard reads
    cat $HACKRIFF_OPS/watchdog.log              # every kill, every alarm transition

State: `$HACKRIFF_OPS/watchdog.json` (one tick, overwritten) and `watchdog.log` (append-only).
Stdlib only, plus `ps` - it must run when psutil does not, and it must never be the thing that
falls over on a loaded box.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
try:
    from alert import notify                      # ops/alert.py, same directory
except Exception:                                  # never let the alert path stop the watchdog
    def notify(level, title, body="", key=None):   # type: ignore[misc]
        return False
from roles import LIVE_ROLES, ROLE_SESSION        # ops/roles.py, the one role->session map

S = os.environ.get("HACKRIFF_OPS") or os.path.expanduser("~/.hackriff-ops")
STATE = os.path.join(S, "watchdog.json")
LOG = os.path.join(S, "watchdog.log")
CLAIMS = os.path.join(S, "work-claims.json")

INTERVAL = int(os.environ.get("WATCHDOG_INTERVAL", "20"))

# --- rule thresholds, all overridable so a test can compress ten minutes into a value ---
UNOWNED_CPU = 90.0          # (a) an unowned process above this %CPU ...
UNOWNED_FOR = 120           #     ... held for this many seconds -> amber
ZOMBIE_CPU = 50.0           # (b) an unowned dead-agent shell above this %CPU ...
ZOMBIE_FOR = 600            #     ... held for this long -> SIGKILL + red
ZOMBIE_SIG = "shell-snapshots/snapshot-zsh"   # the 2026-09-22 signature; nothing else is killed
DASH_CPU = 200.0            # (d) the dashboard's own ceiling
DASH_RSS_MB = 1536.0
DASH_FOR = 120
LOAD_FOR = 300              # (e) load over budget for five minutes
LIVENESS_EVERY = 60         # (f) check the role sessions at most once a minute ...
LIVENESS_MISSES = 2         #     ... relaunch after this many consecutive missed checks ...
RELAUNCH_GAP = 600          #     ... and never the same role twice inside ten minutes
LAUNCH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "launch.sh")
CLAUDE_RE = re.compile(r"(\S*/)?claude(\s|$)")

#: Core budget per owner, from ops/README.md: the gate's reserve is 14, a worker is bounded to 3
#: by cpulimit, a role session to `ROLE_CPU_PCT` (800 = 8). `sccache` gets the build reserve it
#: actually spends on behalf of whoever is compiling - its rustc children are reparented to the
#: server daemon, so from `ps` alone they cannot be charged to the worker that asked for them.
#: macOS itself, the user's desktop apps and the cloudflared tunnels are EXOGENOUS: they get no
#: budget line and are excluded from the plan, because the alarm is about OUR bounds holding and
#: a busy Chrome is not a bound we set. A Claude session we did not launch is NOT exogenous - it
#: is a session on this box spending this box's cores, budgeted like a role session.
WORKER_BUDGET = 3.0
ROLE_BUDGET = 8.0
HEADROOM = 4.0
BUDGET = {"gate": 14.0, "merge-runner": 1.0, "work-runner": 1.0, "sccache": 6.0,
          "dashboard": 1.0, "demo": 1.0, "fuzz-rig": 3.0, "watchdog": 0.5,
          "claude-other": ROLE_BUDGET}
EXOGENOUS = ("system", "apps", "tunnel")


# ---------------------------------------------------------------- process table (pure)
def etime_seconds(s: str) -> int:
    """`ps` ELAPSED -> seconds. Formats: `MM:SS`, `HH:MM:SS`, `DD-HH:MM:SS`."""
    s = s.strip()
    days = 0
    if "-" in s:
        d, _, s = s.partition("-")
        try:
            days = int(d)
        except ValueError:
            return 0
    parts = s.split(":")
    try:
        nums = [int(p) for p in parts]
    except ValueError:
        return 0
    while len(nums) < 3:
        nums.insert(0, 0)
    h, m, sec = nums[-3:]
    return days * 86400 + h * 3600 + m * 60 + sec


PS_FIELDS = ["pid", "ppid", "pgid", "pcpu", "rss", "etime", "command"]


def parse_ps(text: str) -> list[dict]:
    """Rows from `ps -axo pid,ppid,pgid,pcpu,rss,etime,command`.

    Kept a pure function over text so the rules can be tested against a synthetic table -
    including tables this machine could not produce on demand, like sixteen 100 % zsh loops.
    A malformed line is skipped, never fatal: `ps` under load truncates.
    """
    rows = []
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("PID"):
            continue
        f = line.split(None, 6)
        if len(f) < 7:
            continue
        try:
            rows.append({"pid": int(f[0]), "ppid": int(f[1]), "pgid": int(f[2]),
                         "cpu": float(f[3]), "rss_mb": round(int(f[4]) / 1024.0, 1),
                         "etime": etime_seconds(f[5]), "cmd": f[6]})
        except ValueError:
            continue
    return rows


def annotate_env(rows: list[dict]) -> list[dict]:
    """Attach `HACKRIFF_ROLE` to the few rows that could be a role session.

    Best-effort and deliberately narrow. macOS does NOT expose another process's environment -
    neither `ps -E`/`ps -axeww` nor psutil's `environ()` returns it here (measured 2026-09-23),
    so on this box the role is answered by the `--append-system-prompt-file` role file instead
    and this adds nothing. It is kept because `ops/launch.sh` exports the variable and because
    the same watchdog on Linux (a Jetson, CI) can read it, where the command line may not carry
    the role at all.
    """
    try:
        import psutil                                    # optional; absent is fine
    except Exception:
        return rows
    for r in rows:
        if "claude" not in r["cmd"] and "launch.sh" not in r["cmd"]:
            continue
        try:
            role = psutil.Process(r["pid"]).environ().get("HACKRIFF_ROLE")
        except Exception:
            continue
        if role:
            r["env"] = f"HACKRIFF_ROLE={role}"
    return rows


def read_ps() -> list[dict]:
    try:
        out = subprocess.run(["ps", "-axo", ",".join(PS_FIELDS)],
                             capture_output=True, text=True, timeout=25).stdout
    except Exception:
        return []
    return annotate_env(parse_ps(out))


# ---------------------------------------------------------------- attribution (pure)
def role_name(cmd: str, env: str = "") -> str:
    """The role a session is running in: `HACKRIFF_ROLE` when ops/launch.sh exported it into the
    environment we can see, else the role file the session was launched with."""
    m = re.search(r"HACKRIFF_ROLE=([\w-]+)", env or "")
    if m:
        return m.group(1)
    m = re.search(r"[/\s]roles/([\w-]+)\.md", cmd)
    if m:
        return m.group(1)
    m = re.search(r"launch\.sh\s+([\w-]+)", cmd)
    return m.group(1) if m else "session"


#: A merge gate is a process whose PROGRAM is the gate - `just gate[-merge] …`, or the `uv run … python
#: -m hkpy.gate` / `python -m hkpy.gate` it execs - never one whose argv merely QUOTES the words.
#: Unanchored, this matched every agent's wait loop (the Bash tool runs `zsh -c '… eval "until !
#: pgrep -f 'just gate' …"'`, so the text is in the shell's argv): 567 false double-gate alarms on
#: 2026-09-23, each window opened within 40 s of such a loop, and the loop's CPU was charged to
#: `gate`. ops/merge-runner.sh's own detection was anchored the same way (`^just gate`) at 06:44.
GATE_RE = re.compile(r"^(\S*/)?just gate(-merge)?(\s|$)"
                     r"|^(\S*/)?uv run\s[^'\"]*-m hkpy\.gate(\s|$)"
                     r"|^(\S*/)?python[0-9.]*\s+-m hkpy\.gate(\s|$)")

#: Fallback only, applied after ancestry has failed: a process the orchestration did not start
#: and cannot have started. A `/usr/bin/git` a worker spawned still belongs to the worker,
#: because ancestry answers first - these prefixes never override an owner, they only replace
#: UNOWNED for things macOS and the user's desktop are running.
SYSTEM_PREFIXES = ("/System/", "/usr/libexec/", "/usr/sbin/", "/usr/bin/", "/sbin/",
                   "/Library/Apple/", "/opt/homebrew/Cellar/", "/usr/local/Cellar/")
APP_PREFIXES = ("/Applications/",)


def anchor_owner(row: dict, claim_pids: dict[int, str]) -> str | None:
    """The owner a process declares BY ITSELF, or None when it must be inherited from an
    ancestor. Order matters: a worker's claim wins over anything its command line says, because
    the claim is the runner's own record of what it started; and the role-session test comes
    before the bare-`claude` test, since a role session is also a `claude` process."""
    cmd = row["cmd"]
    for key in (row["pid"], row["pgid"]):
        if key in claim_pids:
            return "worker:" + claim_pids[key]
    if "ops/watchdog.py" in cmd:
        return "watchdog"
    if "ops/monitor.py" in cmd:
        return "dashboard"
    if "ops/merge-runner.sh" in cmd:
        return "merge-runner"
    if "ops/work-runner.py" in cmd:
        return "work-runner"
    if "ops/fuzz-rig.sh" in cmd:
        return "fuzz-rig"
    if "ops/stage.sh" in cmd or "hk serve --bind 127.0.0.1:8899" in cmd:
        return "demo"
    # The explorer window (T-923): its window script, its agent, and the server it runs on the HackRF (:8897, data under
    # $HACKRIFF_OPS/explorer/) - started detached by the agent, so ancestry alone never finds it (2026-09-25 04:0x: the
    # live-HackRF server alarmed 'unowned at 387 %, kill it' mid-window).
    if ("ops/explorer-window.sh" in cmd or "--agent explorer" in cmd or "127.0.0.1:8897" in cmd
            or "/.hackriff-ops/explorer/" in cmd):
        return "explorer"
    if "--append-system-prompt-file" in cmd or "ops/launch.sh" in cmd or row.get("env"):
        return "role:" + role_name(cmd, row.get("env", ""))
    return None


def fallback_owner(row: dict) -> str | None:
    """Applied ONLY after ancestry has failed, so it can never steal a process from its real
    owner. Each entry here was measured as a false UNOWNED on 2026-09-23 - the rules below are
    about processes nobody on this box can account for, and an alarm that fires on every compile
    and every Chrome tab is one nobody reads."""
    cmd = row["cmd"]
    if cmd.startswith(APP_PREFIXES):
        return "apps"
    if cmd.startswith(SYSTEM_PREFIXES):
        return "system"
    # sccache reparents every rustc it runs to its own server daemon (measured: one rustc at
    # 429 % whose ppid was the sccache server, not the worker that asked for it). A `sccache
    # rustc` still reachable from a worker's cargo keeps that worker - only what hangs off the
    # detached server lands here.
    if re.search(r"/sccache(\s|$)", cmd) or cmd.startswith("rustc ") or "/bin/rustc " in cmd:
        return "sccache"
    if cmd.startswith("cloudflared ") or "cloudflared tunnel" in cmd:
        return "tunnel"
    # ops/launch.sh attaches a role session's limiter by pid (`cpulimit -i -p <pane pid>`), detached
    # from the pane so it cannot take the terminal: it hangs off launchd, at ~1 % CPU.
    if re.search(r"^(\S*/)?cpulimit\s(?!.*\s--\s).*\s-p\s+\d+\s*$", cmd):
        return "limiter"
    # A Claude session this orchestration did not launch - the user's own terminal. Naming it
    # matters for rule (b): its shells have a LIVE parent and reach it by ancestry, so they are
    # owned and never killed; only a shell whose session has EXITED falls through to UNOWNED,
    # which is exactly the 2026-09-22 incident and nothing else.
    if re.match(r"(\S*/)?claude(\s|$)", cmd):
        return "claude-other"
    return None


def gate_roots(rows: list[dict]) -> list[dict]:
    """The distinct merge gates running: a matching process whose nearest matching ancestor does
    not exist. Counting matching PROCESSES would say four (`just gate` -> `uv run` -> `python -m
    hkpy.gate` are one gate); counting process GROUPS is nearly right but splits on any `set -m`
    subshell. The root is the gate."""
    by_pid = {r["pid"]: r for r in rows}
    roots = []
    for r in rows:
        if not GATE_RE.search(r["cmd"]):
            continue
        p, hops, nested = by_pid.get(r["ppid"]), 0, False
        while p is not None and hops < 64:
            if GATE_RE.search(p["cmd"]):
                nested = True
                break
            p, hops = by_pid.get(p["ppid"]), hops + 1
        if not nested:
            roots.append(r)
    return roots


def attribute(rows: list[dict], claims: dict | None = None) -> dict[int, str]:
    """pid -> owner for every row. Unattributable processes are simply absent from the result;
    `owners()` reports them as UNOWNED rather than folding them into a neighbour."""
    claims = claims or {}
    claim_pids: dict[int, str] = {}
    claim_wts: list[tuple[str, str]] = []
    for tid, c in claims.items():
        if isinstance(c, dict) and c.get("state") == "running" and c.get("pid"):
            try:
                claim_pids[int(c["pid"])] = str(c.get("ticket") or tid)
            except (TypeError, ValueError):
                pass
            if c.get("wt"):
                claim_wts.append((str(c["wt"]).rstrip("/") + "/", str(c.get("ticket") or tid)))

    by_pid = {r["pid"]: r for r in rows}
    direct = {r["pid"]: o for r in rows if (o := anchor_owner(r, claim_pids))}
    # The gate is its own owner even under the merge runner that started it, because the 14-core
    # reserve is the GATE's, not the runner's: folding it into `merge-runner` would hide both the
    # reserve and an orphan gate in the same line.
    for g in gate_roots(rows):
        direct.setdefault(g["pid"], "gate")

    resolved: dict[int, str] = {}

    def owner_of(pid: int, depth: int = 0) -> str | None:
        if pid in resolved:
            return resolved[pid]
        if pid <= 1 or depth > 64 or pid not in by_pid:
            return None
        if pid in direct:
            resolved[pid] = direct[pid]
            return resolved[pid]
        row = by_pid[pid]
        up = owner_of(row["ppid"], depth + 1)
        if up is None and row["pgid"] != pid:
            up = owner_of(row["pgid"], depth + 1)
        # A worker's background shell is reparented to launchd, so its test binaries and servers lose
        # the ancestry to the claim; they still RUN FROM its worktree (2026-09-24: T-577's own
        # degenerate_null test, T-882's and T-901's twice - each alarmed 'unowned, kill it').
        if up is None:
            up = next(("worker:" + t for wt, t in claim_wts if wt in row["cmd"]), None)
        if up is None:
            up = fallback_owner(row)
        if up is not None:
            resolved[pid] = up
        return up

    for r in rows:
        owner_of(r["pid"])
    return resolved


def owners(rows: list[dict], claims: dict | None = None) -> tuple[dict, list[dict]]:
    """({owner: {cpu, rss, pids}}, [unowned rows]) - the shape written to watchdog.json."""
    attr = attribute(rows, claims)
    agg: dict[str, dict] = {}
    unowned: list[dict] = []
    for r in rows:
        if r["pid"] <= 1 or r["ppid"] == 0:
            continue                                    # launchd and the kernel are not ours
        o = attr.get(r["pid"])
        if o is None:
            unowned.append(r)
            continue
        a = agg.setdefault(o, {"cpu": 0.0, "rss": 0.0, "pids": []})
        a["cpu"] += r["cpu"]
        a["rss"] += r["rss_mb"]
        a["pids"].append(r["pid"])
    for a in agg.values():
        a["cpu"] = round(a["cpu"], 1)
        a["rss"] = round(a["rss"], 1)
    return agg, unowned


def budget(agg: dict, cores: int | None = None) -> float:
    """The box's plan for the owners currently present, plus headroom (ops/README.md).

    Capped at the core count, because the plan can exceed the machine and often does: the README
    itself notes "gate 14 + workers 4x3 + role session 8 = 34 at peak, which the QoS tiers
    arbitrate" on a 28-core box, and adds the rule this cap encodes - "a sustained load above
    ~28 means a bound is not holding". Uncapped, the plan would always exceed the load and the
    alarm could never fire; capped, it stays meaningful at both ends - with only the dashboard
    and the demo running, a load of 20 is over budget and something unowned is eating the box.
    """
    cores = cores or os.cpu_count() or 28
    total = 0.0
    for name in agg:
        if name in EXOGENOUS:
            continue
        if name.startswith("worker:"):
            total += WORKER_BUDGET
        elif name.startswith("role:"):
            total += ROLE_BUDGET
        else:
            total += BUDGET.get(name, 1.0)
    return min(total, float(cores)) + HEADROOM


# ---------------------------------------------------------------- rules (pure over `since`)
def _held(since: dict, key: str, cond: bool, now: float, seconds: float) -> bool:
    """True once `cond` has been continuously true for `seconds`. `since` is the carried state:
    a condition that lapses for one tick starts its clock again, so a spike is not an alarm."""
    if not cond:
        since.pop(key, None)
        return False
    t0 = since.setdefault(key, now)
    return now - t0 >= seconds


def top_consumers(rows: list[dict], attr: dict[int, str], n: int = 5) -> list[str]:
    hot = sorted((r for r in rows if r["pid"] > 1), key=lambda r: -r["cpu"])[:n]
    return [f"{r['cpu']:.0f}% [{attr.get(r['pid'], 'UNOWNED')}] pid {r['pid']} {r['cmd'][:90]}"
            for r in hot]


def evaluate(rows: list[dict], agg: dict, unowned: list[dict], load1: float,
             since: dict, now: float, claims: dict | None = None) -> tuple[list[dict], list[dict]]:
    """(alarms, rows_to_kill). Pure given `since`; the caller does the killing and the alerting,
    so every rule can be driven from a synthetic table in a test."""
    attr = attribute(rows, claims)
    alarms: list[dict] = []
    kills: list[dict] = []
    live = set()

    for r in unowned:
        k = f"unowned:{r['pid']}"
        live.add(k)
        if _held(since, k, r["cpu"] > UNOWNED_CPU, now, UNOWNED_FOR):
            alarms.append({"rule": "unowned-cpu", "level": "amber", "key": f"watchdog:{k}",
                           "title": f"unowned process at {r['cpu']:.0f}% CPU",
                           "body": f"pid {r['pid']} (ppid {r['ppid']}), up {r['etime']}s, "
                                   f"{r['rss_mb']} MB, belongs to no worker/gate/role/demo:\n"
                                   f"`{r['cmd'][:400]}`\nNothing on this box claims it. "
                                   f"If it is a dead agent's leftover, kill it."})
        kk = f"zombie:{r['pid']}"
        live.add(kk)
        if ZOMBIE_SIG in r["cmd"] and _held(since, kk, r["cpu"] > ZOMBIE_CPU, now, ZOMBIE_FOR):
            kills.append(r)

    if kills:
        alarms.append({"rule": "zombie-shell", "level": "red", "key": "watchdog:zombie-shell",
                       "title": f"killed {len(kills)} orphaned agent shell loop(s)",
                       "body": "A finished agent left these spinning (the 2026-09-22 failure: 16 of "
                               "them at 100 % for 2 h 18 m, through every merge gate). SIGKILLed:\n"
                               + "\n".join(f"pid {r['pid']} {r['cpu']:.0f}% {r['etime']}s "
                                           f"`{r['cmd'][:160]}`" for r in kills[:10]),
                       "pids": [r["pid"] for r in kills]})

    gates = gate_roots(rows)
    if len(gates) > 1:
        alarms.append({"rule": "double-gate", "level": "red", "key": "watchdog:double-gate",
                       "title": f"{len(gates)} merge gates running at once",
                       "body": "A killed runner's orphan gate beside its replacement's (or an "
                               "agent gating in a worktree): they fight for the same 14 reserved "
                               "cores and the same e2e ports, and neither result can be trusted.\n"
                               + "\n".join(f"pid {g['pid']} pgid {g['pgid']} up {g['etime']}s "
                                           f"`{g['cmd'][:140]}`" for g in gates)
                               + "\nStop the orphan: `kill -- -<pgid>`.",
                       "pids": [g["pid"] for g in gates]})

    d = agg.get("dashboard")
    live.add("dashboard")
    if d and _held(since, "dashboard", d["cpu"] > DASH_CPU or d["rss"] > DASH_RSS_MB, now, DASH_FOR):
        alarms.append({"rule": "dashboard", "level": "amber", "key": "watchdog:dashboard",
                       "title": f"dashboard at {d['cpu']:.0f}% CPU / {d['rss']:.0f} MB",
                       "body": f"ops/monitor.py pids {d['pids']}. Ceiling is {DASH_CPU:.0f}% / "
                               f"{DASH_RSS_MB:.0f} MB; on 2026-09-22 it reached 440 % and 2.1 GB "
                               f"and stopped answering. Restart it."})

    plan = budget(agg, os.cpu_count())
    live.add("load")
    if _held(since, "load", load1 > plan, now, LOAD_FOR):
        alarms.append({"rule": "over-budget", "level": "amber", "key": "watchdog:over-budget",
                       "title": f"box over budget: load {load1:.1f} vs plan {plan:.0f}",
                       "body": "Owners: " + ", ".join(f"{k} {v['cpu']:.0f}%" for k, v in
                                                      sorted(agg.items(), key=lambda kv: -kv[1]["cpu"]))
                               + "\nTop 5:\n" + "\n".join(top_consumers(rows, attr))})
        since.setdefault("ob:start", now)
        since["ob:peak"] = max(since.get("ob:peak", 0.0), load1)
    # ONE 'recovered' line per episode (supervisor 2026-09-25 01:52: the user sleeps; an episode is one alarm - the
    # key's dedupe - and one all-clear). Held under plan as long as the alarm needed to fire; its own key per episode,
    # outside the prefixes that wake the pipeline manager, so it reaches Discord and pages nobody.
    live.add("load-ok")
    if "ob:start" in since and _held(since, "load-ok", load1 <= plan, now, LOAD_FOR):
        start, peak = since.pop("ob:start"), since.pop("ob:peak", load1)
        alarms.append({"rule": "recovered", "level": "green", "key": f"recovered:over-budget:{int(start)}",
                       "title": f"box load recovered: {load1:.1f} vs plan {plan:.0f}",
                       "body": f"over budget from {time.strftime('%H:%M', time.localtime(start))} for "
                               f"{(now - start) / 60:.0f} min, peak {peak:.1f}"})

    for k in [k for k in since if k.split(":")[0] in ("unowned", "zombie") and k not in live]:
        since.pop(k, None)                          # the process is gone; forget its clock
    return alarms, kills


# ---------------------------------------------------------------- role-session liveness
# WHY (2026-09-24 04:07): a supervisor `pkill` took the coordinator (`dev`) and the pipeline
# manager (`flow`) down with five workers, and nothing noticed for five and a half hours - every
# runner kept going, and the alerts they relay were typed into sessions that no longer existed.
def _tmux(*args: str) -> subprocess.CompletedProcess | None:
    try:
        return subprocess.run(["tmux", *args], capture_output=True, text=True, timeout=10)
    except Exception:
        return None


def session_missing(session: str, rows: list[dict]) -> str | None:
    """"" when `session` exists and runs claude; what is missing when it does not; None when we
    cannot tell. `=` makes the target exact: tmux prefix-matches `-t dev` against any `dev…`.
    ops/launch.sh `exec`s claude into the pane, so the pane pid is normally claude itself, but a
    claude below it counts too. A `remain-on-exit` corpse is `#{pane_dead}` = 1. A live pane whose
    pid is not in this tick's ps table is UNKNOWN: the table is older than the pane."""
    r = _tmux("has-session", "-t", f"={session}")
    if r is None:
        return None
    if r.returncode != 0:
        return "no tmux session"
    r = _tmux("list-panes", "-s", "-t", f"={session}", "-F", "#{pane_pid} #{pane_dead}")
    if r is None or r.returncode != 0:
        return None                                  # a failed query is not a dead session
    panes = [ln.split() for ln in r.stdout.splitlines() if len(ln.split()) == 2]
    live = {int(p) for p, dead in panes if dead != "1" and p.isdigit()}
    if not live:
        return "pane dead (claude exited; remain-on-exit kept it)" if panes else None
    by_pid = {row["pid"]: row for row in rows}
    if any(p not in by_pid for p in live):
        return None
    kids: dict[int, list[dict]] = {}
    for row in rows:
        kids.setdefault(row["ppid"], []).append(row)
    todo = [by_pid[p] for p in live]
    seen: set[int] = set()
    while todo:
        row = todo.pop()
        if row["pid"] in seen:
            continue
        seen.add(row["pid"])
        if CLAUDE_RE.match(row["cmd"]):
            return ""
        todo.extend(kids.get(row["pid"], []))
    return "session exists, no claude process in its pane"


def relaunch(role: str, session: str, kill_first: bool, dry: bool) -> str:
    """Kill a claude-less session (ops/launch.sh refuses an existing one) after saving its last
    screen to the log - a failed launch's error is on it - then launch the role in its own
    process group. Returns what happened, for the alert and the log."""
    steps = [f"tmux kill-session -t ={session}"] if kill_first else []
    steps.append(f"{LAUNCH} {role}")
    if dry:
        logline(f"DRY-RUN would relaunch {role}: " + " && ".join(steps))
        return "dry-run: would run " + " && ".join(steps)
    if kill_first:
        r = _tmux("capture-pane", "-p", "-t", f"={session}")
        screen = "\n".join((r.stdout.rstrip().splitlines() if r else [])[-40:])
        logline(f"PANE {session} before kill-session:\n{screen}")
        _tmux("kill-session", "-t", f"={session}")
    try:
        p = subprocess.run([LAUNCH, role], capture_output=True, text=True, timeout=60,
                           stdin=subprocess.DEVNULL, start_new_session=True)
        rc, out = p.returncode, (p.stdout + p.stderr).strip()
    except Exception as e:
        rc, out = -1, f"{type(e).__name__}: {e}"
    tail = " | ".join(out.splitlines()[-4:])
    logline(f"RELAUNCH {role} session={session} killed_first={kill_first} rc={rc} -- {tail[:600]}")
    return f"ran {' && '.join(steps)}: exit {rc}\n{tail}"


#: A deliberate stop: `/dev-env stop` writes roles-stopped, the full stop is dispatch-paused.
#: Either one means the sessions are down on purpose - alert, never relaunch.
STOP_MARKERS = ("dispatch-paused", "roles-stopped")
RELAUNCH_TRIES = 3          # relaunches that came back dead on the next check, then give up


def liveness(rows: list[dict], since: dict, now: float, dry: bool = False) -> list[dict]:
    """(f) Red for each LIVE_ROLES session found dead; on the LIVENESS_MISSES-th consecutive miss,
    relaunch it - at most once per RELAUNCH_GAP, never while a stop marker exists, and not after
    RELAUNCH_TRIES relaunches that each came back dead. Counts live in `since`, like the rule
    clocks, so they reset with the watchdog."""
    if not rows or now - since.get("liveness:at", -1e18) < LIVENESS_EVERY:
        return []                                    # no ps table is "unknown", never "dead"
    since["liveness:at"] = now
    stopped = [m for m in STOP_MARKERS if os.path.exists(os.path.join(S, m))]
    alarms: list[dict] = []
    for role in LIVE_ROLES:
        session = ROLE_SESSION[role]
        missing = session_missing(session, rows)
        mk, lk = f"liveness:miss:{role}", f"liveness:launched:{role}"
        vk, fk = f"liveness:verify:{role}", f"liveness:failed:{role}"
        if missing is None:
            continue
        if not missing:
            for k in (mk, vk, fk):
                since.pop(k, None)
            continue
        if since.pop(vk, None):                      # the last relaunch came back dead
            since[fk] = since.get(fk, 0) + 1
        n = since[mk] = since.get(mk, 0) + 1
        a = {"rule": "liveness", "level": "red", "key": f"watchdog:liveness:{role}",
             "title": f"{role} is dead: tmux session '{session}': {missing}",
             "body": f"Missed check {n} of {LIVENESS_MISSES} before a relaunch."}
        alarms.append(a)
        if n < LIVENESS_MISSES:
            continue
        if stopped:
            a["body"] += f" Not relaunching: {', '.join(stopped)} exists (a deliberate stop)."
            continue
        if since.get(fk, 0) >= RELAUNCH_TRIES:
            a["body"] += (f" giving up relaunching {role} after {RELAUNCH_TRIES} attempts - "
                          f"see watchdog.log")
            continue
        ago = now - since.get(lk, -1e18)
        if ago < RELAUNCH_GAP:
            a["body"] += (f" Not relaunching: relaunched {ago:.0f}s ago (at most once per "
                          f"{RELAUNCH_GAP}s). Relaunch by hand: ops/launch.sh {role}")
            continue
        kill_first = missing != "no tmux session"
        if kill_first:                               # this tick's ps may be stale: look again
            again = session_missing(session, read_ps())
            if not again:
                logline(f"LIVENESS {role}: claude present or unknown on re-read ({again!r}); no kill")
                since.pop(mk, None)
                alarms.remove(a)
                continue
        what = relaunch(role, session, kill_first, dry)
        since[lk] = now
        since[vk] = True
        since.pop(mk, None)
        alarms.append({"rule": "relaunch", "level": "red", "key": f"watchdog:relaunch:{role}",
                       "title": f"relaunched {role} in tmux session '{session}'"
                                + (" (killed the claude-less session first)" if kill_first else ""),
                       "body": what})
    return alarms


# ---------------------------------------------------------------- tick
def logline(msg: str) -> None:
    try:
        os.makedirs(S, exist_ok=True)
        with open(LOG, "a") as f:
            f.write(f"{time.strftime('%Y-%m-%d %H:%M:%S')} {msg}\n")
    except Exception:
        pass


def load_claims() -> dict:
    try:
        return json.load(open(CLAIMS))
    except Exception:
        return {}


def kill_now(rows: list[dict]) -> list[int]:
    """SIGKILL, not SIGTERM: a `zsh -c` busy loop has no handler to run and the whole point is
    that its parent is already gone. Every kill is logged with its full command line."""
    done = []
    for r in rows:
        if ZOMBIE_SIG not in r["cmd"]:
            continue                                # belt and braces: never kill off-signature
        try:
            os.kill(r["pid"], signal.SIGKILL)
            done.append(r["pid"])
            logline(f"KILL pid={r['pid']} ppid={r['ppid']} cpu={r['cpu']} etime={r['etime']}s cmd={r['cmd'][:300]}")
        except OSError as e:
            logline(f"KILL-FAILED pid={r['pid']} {e}")
    return done


# ---------------------------------------------------------------- (h) worktree orphans
# WHY (2026-09-25): a `zsh -c 'cargo build -p hk-cli --bin hk | grep | head'` in
# .claude/worktrees/t901 ran 15 h with ppid 1 and no claim, and the cargo-nextest runs of the
# sessions killed at 09:34 kept going in t926/t940/t950-red/t953. None was over 90 % CPU, so rule
# (a) never even alarmed, and nothing but a person with `ps` could stop them.
ORPHAN_FOR = 600
#: The work runner's live claim states (ops/work-runner.py): a review runs as "running" too, and a
#: "fix-held" claim resumes in its worktree. Host does not matter: a node2 claim's `wt` is the Mac path.
ORPHAN_STATES = ("running", "fix-held")
#: Fallback labels, not owners: an orphan cargo's sccache rustc must not protect its own worktree.
NOT_AN_OWNER = ("sccache", "system", "apps", "tunnel", "limiter")
WT_RE = re.compile(r"/[^\s'\";&|()]*?/\.claude/worktrees/[^/\s'\";&|()]+")
BUILD_RE = re.compile(r"^(\S*/)?(cargo|cargo-nextest|rustc)(\s|$)|^(\S*/)?hk\s+serve(\s|$)"
                      r"|^(\S*/)?node\s+\S*e2e/\S+\.mjs|^/\S*/\.claude/worktrees/[^/\s]+/target/")
SHELL_RE = re.compile(r"^(\S*/)?(zsh|bash|sh)\s(.*\s)?-c\s")
WRAPS_BUILD_RE = re.compile(r"(^|[\s;&|('\"])(\S*/)?(cargo|cargo-nextest|rustc)(\s|$)|hk serve"
                            r"|e2e/\S+\.mjs|/\.claude/worktrees/[^/\s]+/target/")


def worktrees_in(text: str) -> list[str]:
    return WT_RE.findall(text or "")


def _build_kind(row: dict, by_pid: dict[int, dict]) -> bool:
    """cargo, cargo-nextest, a rustc no sccache runs, `hk serve`, node running ui/e2e, a binary
    under a worktree's target/, or a `sh -c` wrapping one of them."""
    cmd = row["cmd"]
    if SHELL_RE.match(cmd):
        return bool(WRAPS_BUILD_RE.search(cmd))
    if not BUILD_RE.match(cmd):
        return False
    if re.match(r"^(\S*/)?rustc(\s|$)", cmd):
        p, hops = by_pid.get(row["ppid"]), 0
        while p is not None and hops < 64:
            if re.search(r"(^|/)sccache(\s|$)", p["cmd"]):
                return False                           # sccache's own rustc stays sccache's
            p, hops = by_pid.get(p["ppid"]), hops + 1
    return True


def orphan_pool(rows: list[dict], attr: dict[int, str]) -> list[dict]:
    """Build/test rows no live owner accounts for: attribution failed, or only the sccache fallback
    named a rustc that no sccache runs."""
    by_pid = {r["pid"]: r for r in rows}
    return [r for r in rows if r["pid"] > 1 and r["ppid"] != 0
            and attr.get(r["pid"]) in (None, "sccache") and _build_kind(r, by_pid)]


def cwd_pids(rows: list[dict], attr: dict[int, str], pool: list[dict]) -> list[int]:
    """Whose cwd the one lsof asks for: the pool rows whose command names no worktree, plus the
    OWNED build, shell and claude rows that name none (an agent's shell there protects it). None
    at all when the pool is empty, so a quiet box never runs lsof."""
    if not pool:
        return []
    by_pid = {r["pid"]: r for r in rows}
    owned = [r for r in rows if attr.get(r["pid"]) not in (None, *NOT_AN_OWNER)
             and (_build_kind(r, by_pid) or SHELL_RE.match(r["cmd"]) or CLAUDE_RE.match(r["cmd"]))]
    return sorted({r["pid"] for r in pool + owned if not worktrees_in(r["cmd"])})


def read_cwds(pids: list[int]) -> dict[int, str]:
    """pid -> cwd from ONE `lsof` call; any failure is simply no cwd (so: not a candidate)."""
    if not pids:
        return {}
    try:
        out = subprocess.run(["lsof", "-a", "-d", "cwd", "-Fpn", "-p", ",".join(map(str, pids))],
                             capture_output=True, text=True, timeout=15).stdout
    except Exception:
        return {}
    cwds, pid = {}, None
    for ln in out.splitlines():
        if ln.startswith("p") and ln[1:].isdigit():
            pid = int(ln[1:])
        elif ln.startswith("n") and pid is not None:
            cwds[pid] = ln[1:]
    return cwds


def protected_worktrees(rows: list[dict], attr: dict[int, str], claims: dict,
                        cwds: dict[int, str]) -> set[str]:
    """A worktree with a live claim (any host), or one an owned process names or runs in."""
    wts = {w for c in claims.values() if isinstance(c, dict) and c.get("state") in ORPHAN_STATES
           for w in worktrees_in(str(c.get("wt") or "") + "/")}
    for r in rows:
        if attr.get(r["pid"]) not in (None, *NOT_AN_OWNER):
            wts.update(worktrees_in(r["cmd"]) or worktrees_in(cwds.get(r["pid"], "")))
    return wts


def kill_orphans(due: list[dict], since: dict, now: float, cwds: dict[int, str]) -> list[dict]:
    """SIGTERM, then SIGKILL on a later tick if it is still there (the t901 zsh ignored SIGTERM).
    Its own belt and braces, separate from kill_now's signature lock: re-read ps and the claims and
    refuse unless the pid still runs the same command, still unowned, in a still-unprotected
    worktree. Every signal is logged with the full command line."""
    fresh = read_ps()
    claims = load_claims()
    attr = attribute(fresh, claims)
    by_pid = {r["pid"]: r for r in fresh}
    pool = {r["pid"] for r in orphan_pool(fresh, attr)}
    protected = protected_worktrees(fresh, attr, claims, cwds)
    done = []
    for r in due:
        now_row = by_pid.get(r["pid"])
        if (now_row is None or now_row["cmd"] != r["cmd"] or r["pid"] not in pool
                or any(w in protected for w in r["wts"])):
            logline(f"KILL-ORPHAN-REFUSED pid={r['pid']} wt={r['wt']}: no longer an unowned orphan "
                    f"in an unprotected worktree")
            continue
        tk = f"orphan-term:{r['pid']}"
        sig = signal.SIGKILL if tk in since else signal.SIGTERM
        try:
            os.kill(r["pid"], sig)
        except OSError as e:
            logline(f"KILL-ORPHAN-FAILED pid={r['pid']} {e}")
            continue
        since[tk] = now
        done.append(dict(r, sig=sig.name))
        logline(f"KILL-ORPHAN {sig.name} pid={r['pid']} ppid={r['ppid']} wt={r['wt']} cpu={r['cpu']} "
                f"etime={r['etime']}s cmd={r['cmd']}")
    return done


def wt_orphans(rows: list[dict], claims: dict, since: dict, now: float,
               dry: bool = False) -> tuple[list[dict], list[int]]:
    """(h) ([alarm], [pids signalled]). A build/test process in a /.claude/worktrees/<name> (from
    its command line, else its cwd) that is unowned, whose worktree no live claim on any host and
    no owned process protects, held so for ORPHAN_FOR, is stopped - one red alert per batch."""
    attr = attribute(rows, claims)
    pool = orphan_pool(rows, attr)
    cwds = read_cwds(cwd_pids(rows, attr, pool))
    protected = protected_worktrees(rows, attr, claims, cwds)
    live, due = set(), []
    for r in pool:
        wts = worktrees_in(r["cmd"]) or worktrees_in(cwds.get(r["pid"], ""))
        if not wts or any(w in protected for w in wts):
            continue
        k = f"orphan:{r['pid']}"
        live.add(k)
        if _held(since, k, True, now, ORPHAN_FOR):
            due.append(dict(r, wt=wts[0], wts=wts))
    for k in [k for k in since if k.startswith(("orphan:", "orphan-term:"))
              and "orphan:" + k.split(":", 1)[1] not in live]:
        since.pop(k, None)
    acted = due if dry else kill_orphans(due, since, now, cwds)
    if not acted:
        return [], []
    wts = sorted({r["wt"] for r in acted})
    verb = "would stop" if dry else "stopped"
    return [{"rule": "wt-orphan", "level": "red", "key": "watchdog:wt-orphan",
             "title": f"{verb} {len(acted)} worktree orphan(s) in {', '.join(w.rsplit('/', 1)[-1] for w in wts)}",
             "body": "No running claim on any host, no owned process there, unowned 10 min (2026-09-25: "
                     "t901's 15-h cargo wrapper; the 09:34 killed sessions' nextest runs):\n"
                     + "\n".join(f"pid {r['pid']} {r.get('sig', 'dry-run')} {r['wt']} up {r['etime']}s "
                                 f"`{r['cmd'][:160]}`" for r in acted[:10]),
             "pids": [r["pid"] for r in acted]}], ([] if dry else [r["pid"] for r in acted])


# ---------------------------------------------------------------- radio lock (T-922)
RADIO_PY = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "py", "hkpy", "radio.py")


def _radio():
    """py/hkpy/radio.py by path (stdlib only), so the watchdog needs no uv environment."""
    import importlib.util
    spec = importlib.util.spec_from_file_location("hk_radio", RADIO_PY)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)  # type: ignore[union-attr]
    return mod


def radio_stale(now: float, dry: bool = False) -> list[dict]:
    """(g) A radio lock past its `until` is released, with a red alert: its owner overran its window
    or died holding the radio, and staging stays on replay until the lock is gone. `--dry-run`
    reports without removing it."""
    try:
        R = _radio()
        lock = R.read(S)
        if lock is None or not R.is_stale(lock, now):
            return []
        what = R.describe(lock, now)
        if not dry:
            R.release_stale(S, now)
    except Exception as e:  # never let the lock path stop the watchdog
        logline(f"radio-lock check failed: {e}")
        return []
    return [{"rule": "radio-stale", "level": "red", "key": "watchdog:radio-stale",
             "title": f"stale radio lock {'would be ' if dry else ''}released: {lock['owner']}",
             "body": f"{what}\nstaging goes back to LIVE on its next tick (ops/stage.sh)."}]


def tick(since: dict, dry: bool = False) -> dict:
    rows = read_ps()
    claims = load_claims()
    agg, unowned = owners(rows, claims)
    try:
        load1 = os.getloadavg()[0]
    except Exception:
        load1 = 0.0
    alarms, kills = evaluate(rows, agg, unowned, load1, since, time.time(), claims)
    alarms += liveness(rows, since, time.time(), dry)
    alarms += radio_stale(time.time(), dry)
    killed = [] if dry else kill_now(kills)
    orphan_alarms, stopped = wt_orphans(rows, claims, since, time.time(), dry)
    alarms += orphan_alarms
    killed += stopped
    snap = {
        "ts": time.time(), "at": time.strftime("%Y-%m-%d %H:%M:%S"),
        "load": round(load1, 2), "budget": round(budget(agg, os.cpu_count()), 1),
        "cores": os.cpu_count() or 1,
        # `n` then a capped pid list: `system` alone is 600+ processes on this box, and the
        # dashboard re-reads this file every 5 s. The count is the fact worth having; the pids
        # are for following one up, and a dozen is enough to start.
        "owners": {k: {"cpu": v["cpu"], "rss": v["rss"], "n": len(v["pids"]), "pids": v["pids"][:12]}
                   for k, v in sorted(agg.items(), key=lambda kv: -kv[1]["cpu"])},
        "unowned": [{"pid": r["pid"], "cpu": r["cpu"], "rss_mb": r["rss_mb"],
                     "etime": r["etime"], "cmd": r["cmd"][:200]}
                    for r in sorted(unowned, key=lambda r: -r["cpu"])[:12] if r["cpu"] >= 1.0],
        "alarms": [{k: a[k] for k in ("rule", "level", "title") } for a in alarms],
        "killed": killed,
    }
    try:
        os.makedirs(S, exist_ok=True)
        tmp = STATE + ".tmp"
        with open(tmp, "w") as f:
            json.dump(snap, f)
        os.replace(tmp, STATE)
    except Exception:
        pass
    for a in alarms:
        if not dry:
            notify(a["level"], a["title"], a.get("body", ""), key=a["key"])
        # The body goes in too: Discord dedupes by key, so from the second tick on the log is the
        # only record of WHICH processes tripped an alarm (567 double-gate alarms on 2026-09-23
        # named no pid anywhere, and by the time anyone looked the matching process was gone).
        body = " | ".join(ln.strip() for ln in a.get("body", "").splitlines() if ln.strip())
        logline(f"ALARM {a['level']} {a['rule']}: {a['title']}" + (f" -- {body[:600]}" if body else ""))
    return snap


CONTENTION_STALE_S = 180


def contention(snap: dict | None, now: float | None = None) -> str:
    """What the box is doing that a merge gate should not share, in one line; "" when clear.

    `ops/merge-runner.sh` calls this (`--contended`) before every gate. Its own `workers_running`
    only knows processes this orchestration started, which is exactly why the sixteen orphaned
    shells of 2026-09-22 ran through every gate with the drain check reporting zero.

    A STALE tick is "nothing known", not "clear" and not "contended": a dead watchdog must not
    silently license a contended gate, and must not block every merge either. The caller's
    45-minute cap is what makes that safe in the one direction, and the cap is why this can
    afford to be conservative in the other.
    """
    if not snap:
        return ""
    now = time.time() if now is None else now
    if now - float(snap.get("ts", 0)) > CONTENTION_STALE_S:
        return ""
    out = [f"pid {u['pid']} {float(u.get('cpu', 0)):.0f}% {str(u.get('cmd', ''))[:70]}"
           for u in snap.get("unowned", []) if float(u.get("cpu", 0)) > UNOWNED_CPU]
    if float(snap.get("load", 0)) > float(snap.get("budget", 1e9)):
        out.append(f"load {snap['load']} over budget {snap['budget']}")
    return " | ".join(out)


def read_snapshot() -> dict:
    try:
        return json.load(open(STATE))
    except Exception:
        return {}


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--contended", action="store_true",
                    help="print what the last tick saw contending the box (empty = clear), then exit")
    ap.add_argument("--once", action="store_true", help="one tick, then exit")
    ap.add_argument("--print", action="store_true", dest="show", help="print the tick as JSON")
    ap.add_argument("--dry-run", action="store_true", help="never kill, never alert")
    a = ap.parse_args(argv)
    if a.contended:
        print(contention(read_snapshot()))
        return 0
    # Point active-ops-dir at ourselves the way the other ops scripts do, so a later cold start
    # finds the same state directory.
    try:
        os.makedirs(S, exist_ok=True)
        open(os.path.expanduser("~/.hackriff-ops/active-ops-dir"), "w").write(S + "\n")
    except Exception:
        pass
    import launchpath
    launchpath.check(__file__, logline)
    logline(f"START pid={os.getpid()} interval={INTERVAL}s ops={S}"
            + (" (dry-run)" if a.dry_run else ""))
    since: dict = {}
    while True:
        try:
            snap = tick(since, dry=a.dry_run)
            if a.show:
                print(json.dumps(snap, indent=2), flush=True)
        except Exception as e:                       # a watchdog that dies is worse than none
            logline(f"TICK-ERROR {type(e).__name__}: {e}")
        if a.once:
            return 0
        time.sleep(INTERVAL)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
