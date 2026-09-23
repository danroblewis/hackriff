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

IT KILLS ALMOST NOTHING. Exactly one rule kills, and only for the signature that produced the
incident: an unowned `shell-snapshots/snapshot-zsh` loop (a dead agent's shell; a live agent's
shell is a descendant of its session and is therefore owned) burning >50 % for >10 minutes.
Everything else is an alert. A watchdog that kills on a guess is worse than the contention it
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
    m = re.search(r"HACKRIFF_ROLE=(\w+)", env or "")
    if m:
        return m.group(1)
    m = re.search(r"[/\s]roles/(\w+)\.md", cmd)
    if m:
        return m.group(1)
    m = re.search(r"launch\.sh\s+(\w+)", cmd)
    return m.group(1) if m else "session"


GATE_RE = re.compile(r"\bjust gate\b|hkpy\.gate\b")

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
    for tid, c in claims.items():
        if isinstance(c, dict) and c.get("state") == "running" and c.get("pid"):
            try:
                claim_pids[int(c["pid"])] = str(c.get("ticket") or tid)
            except (TypeError, ValueError):
                pass

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

    for k in [k for k in since if k.split(":")[0] in ("unowned", "zombie") and k not in live]:
        since.pop(k, None)                          # the process is gone; forget its clock
    return alarms, kills


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


def tick(since: dict, dry: bool = False) -> dict:
    rows = read_ps()
    claims = load_claims()
    agg, unowned = owners(rows, claims)
    try:
        load1 = os.getloadavg()[0]
    except Exception:
        load1 = 0.0
    alarms, kills = evaluate(rows, agg, unowned, load1, since, time.time(), claims)
    killed = [] if dry else kill_now(kills)
    snap = {
        "ts": time.time(), "at": time.strftime("%Y-%m-%d %H:%M:%S"),
        "load": round(load1, 2), "budget": round(budget(agg, os.cpu_count()), 1),
        "cores": os.cpu_count() or 1,
        "owners": {k: v for k, v in sorted(agg.items(), key=lambda kv: -kv[1]["cpu"])},
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
        logline(f"ALARM {a['level']} {a['rule']}: {a['title']}")
    return snap


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--once", action="store_true", help="one tick, then exit")
    ap.add_argument("--print", action="store_true", dest="show", help="print the tick as JSON")
    ap.add_argument("--dry-run", action="store_true", help="never kill, never alert")
    a = ap.parse_args(argv)
    # Point active-ops-dir at ourselves the way the other ops scripts do, so a later cold start
    # finds the same state directory.
    try:
        os.makedirs(S, exist_ok=True)
        open(os.path.expanduser("~/.hackriff-ops/active-ops-dir"), "w").write(S + "\n")
    except Exception:
        pass
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
