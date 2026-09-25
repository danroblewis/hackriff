#!/usr/bin/env python3
"""Deterministic work runner - NO AI in the dispatch path.

The merge runner (ops/merge-runner.sh) proved the shape: a script does the mechanical thing
from a file and escalates only exceptions. Dispatch has the same shape, and the coordinator
doing it by hand was the bottleneck - one serial Opus loop deciding, per tick, whether to
start a ticket, while builder slots sat idle for the length of a gate (2026-09-22: zero
workers running while a 30-minute gate ran, with the cap allowing three).

Per tick this runner:
  1. REAPS finished workers: a branch with commits ahead of main goes to the reviewer stage
     (core-interface tickets and cheap-model output, per CLAUDE.md) or straight to the merge
     queue; anything else (no commits, error, timeout, uncommitted tree) is written to
     work-needs-attention.txt for a person or the coordinator.
  2. SYNCS the board: started tickets -> `in-progress` (+branch), landed tickets -> `done`
     (+merge sha), in one small commit on main, only when main is safe to commit to (no
     MERGE_HEAD, no bulk-in-progress, clean tree) - the same rule the coordinator follows.
  3. DISPATCHES: picks `todo` tickets whose deps are done, not blocked, not needing the user
     or hardware, one per parallel_group at a time, user-requested first then priority then
     number; creates the worktree + branch, seeds the build target (APFS clone), writes a
     brief, and launches `claude -p --agent worker` detached with the ticket's model/effort.

State lives in $HACKRIFF_OPS (the merge runner's pointer at ~/.hackriff-ops/active-ops-dir):
  work-claims.json           running/finished claims (the live "who is on what")
  work-needs-attention.txt   exceptions, one line each, for the coordinator's tick
  work-done.jsonl            one line per finished run: minutes, cost, turns, outcome
  work/<ticket>/             brief.md, out.json (claude -p result), run.log, review.json
  work-runner.log            this script's log

Builder cap: CLAUDE.md allows 4 Rust-building agents INCLUDING a running gate, so the cap is
4 minus one while the merge runner is gating. Disk floor 20 GB (df, not du).

Usage: python3 ops/work-runner.py [--once] [--dry-run] [--poll SECONDS]
"""
import argparse
import json
import zlib
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid

import yaml

REPO = "/Users/daniellewis/hackriff"
_PTR = os.path.expanduser("~/.hackriff-ops/active-ops-dir")
S = os.environ.get("HACKRIFF_OPS") or (open(_PTR).read().strip() if os.path.exists(_PTR) else os.path.expanduser("~/.hackriff-ops"))
os.makedirs(S, exist_ok=True)
CLAIMS = f"{S}/work-claims.json"
NEEDS = f"{S}/work-needs-attention.txt"
DONE = f"{S}/work-done.jsonl"
LOG = f"{S}/work-runner.log"
WORKDIR = f"{S}/work"
MERGE_QUEUE = f"{S}/merge-queue.txt"
BULKMARK = f"{S}/bulk-in-progress"
LANDED = f"{S}/landed.jsonl"
# The 3rd isolation-pass of one test within 7 days (user, 2026-09-23) makes py/hkpy/flakes.py append
# a request here; this runner dispatches a deflaker for it (see dispatch_deflakes).
DEFLAKE_REQUESTS = f"{S}/deflake-requests.jsonl"

# THE RESOURCE MODEL IS A FIXED BUDGET, NOT A HEURISTIC (user, 2026-09-22). This box has 28 cores
# (M3 Ultra: 20 performance + 8 efficiency). The merge gate is reserved 14 (its 6 build jobs + 8 test
# threads, on P-cores, never contended). Each worker is BOUNDED, and the bound is inherited by its
# whole process tree: CARGO_BUILD_JOBS and NEXTEST_TEST_THREADS (environment - every rustc and test
# runner it spawns obeys them) plus a `taskpolicy -c background` QoS clamp at launch, which on Apple
# Silicon confines the tree to the efficiency cores. So a worker costs ~WORKER_CORES, the count is
# CAP, and the gate always has its reserve. A cpulimit FORK (see launch()) is the hard ceiling on
# top. No load-average admission, no gate-time throttling, no suspend/resume: a known bound per
# worker is the whole mechanism.
# THE KNOB STORE (pipeline manager, 2026-09-23): `$HACKRIFF_OPS/env` (KEY=VALUE lines, written by
# `just knobs set`) is read before the defaults below, so an experiment's setting survives a plain
# restart. The process environment wins over the store: a one-off override on the command line is
# deliberate and is not silently replaced.
def _load_knob_store() -> None:
    try:
        with open(f"{S}/env", encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line or line.startswith("#") or "=" not in line:
                    continue
                k, v = line.split("=", 1)
                os.environ.setdefault(k.strip(), v.strip())
    except FileNotFoundError:
        pass


_load_knob_store()
CORES = int(os.environ.get("WORK_CORES", "28"))
GATE_RESERVE = int(os.environ.get("WORK_GATE_RESERVE", "14"))
WORKER_JOBS = os.environ.get("WORK_WORKER_JOBS", "2")            # cargo build jobs per worker
WORKER_TEST_THREADS = os.environ.get("WORK_WORKER_TEST_THREADS", "2")
WORKER_CORES = int(os.environ.get("WORK_WORKER_CORES", "3"))    # what one worker may occupy at peak
CAP = int(os.environ.get("WORK_CAP", str(max(1, (CORES - GATE_RESERVE) // WORKER_CORES))))
CPULIMIT = os.environ.get("WORK_CPULIMIT", f"{S}/bin/cpulimit")   # the HiGarfield fork; see launch()
PER_TICK = int(os.environ.get("WORK_PER_TICK", "2"))
LOAD_MAX = float(os.environ.get("WORK_LOAD_MAX", "40"))   # a tripwire only; the budget is the mechanism
# Dispatch pauses once this many branches wait in merge-queue.txt (user, 2026-09-22): the
# running workers drain, the box empties, and the merge gate runs the batch alone.
QUEUE_PAUSE = int(os.environ.get("WORK_QUEUE_PAUSE", "6"))
# Tickets in one parallel_group share a crate, not necessarily a file. Serialising a whole group
# behind one ticket held 18 hk-pipeline tickets idle on 2026-09-22; a real conflict costs one
# re-merge (the merge runner skips the conflicting branch), so allow a few per group.
GROUP_CAP = int(os.environ.get("WORK_GROUP_CAP", "2"))
DISK_MIN_GB = int(os.environ.get("WORK_DISK_MIN_GB", "20"))
CLONE_TARGET = os.environ.get("WORK_CLONE_TARGET", "1") != "0"   # clone main's target/ into a new worktree
REAP_AFTER_MIN = int(os.environ.get("WORK_REAP_AFTER_MIN", "30"))   # a worktree younger than this is never reaped
IDLE_TARGET_H = float(os.environ.get("WORK_IDLE_TARGET_H", "2"))     # a kept worktree's target/ untouched this long is reclaimed
MAX_MINUTES = int(os.environ.get("WORK_MAX_MINUTES", "180"))
REVIEW_MAX_MINUTES = int(os.environ.get("WORK_REVIEW_MAX_MINUTES", "45"))
# A branch that fails its merge gate goes back to the SAME worker: `claude -p --resume <session>`
# with the failure, so the agent that wrote the code fixes it with its context intact, instead of
# a fresh agent (or the coordinator) rediscovering everything. Capped like the merge runner's own
# attempts; the coordinator hears about it only when the cap is spent.
FIX_ATTEMPTS = int(os.environ.get("WORK_FIX_ATTEMPTS", "2"))
KILL_RESUMES = 2   # resumes of a run killed by a signal; not fix attempts - nothing failed
# A work run that hits MAX_MINUTES is resumed ONCE to wrap up (commit what is done, hand back) instead of parking
# for a person: five did on 2026-09-24 (T-852, T-878, T-888, T-887, T-904), each finished by hand afterwards.
TIMEOUT_RESUMES = 1
# A claim that ended in NO_WORK / ERROR / TIMEOUT is released after this long if the ticket is still
# todo, so an accident (a killed process, a crashed worker) cannot freeze a ticket for ever. BLOCKED
# and review/gate escalations are NOT released: those need a person.
RELEASE_AFTER_H = float(os.environ.get("WORK_RELEASE_AFTER_H", "4"))
MERGE_NEEDS = f"{S}/merge-needs-attention.txt"
MERGE_LOG = f"{S}/merge-runner.log"
BUDGET_USD = os.environ.get("WORK_BUDGET_USD", "20")
MODEL_ALIAS = {"haiku": "haiku", "sonnet": "sonnet", "opus": "opus", "fable": "claude-fable-5-1"}
EFFORTS = ("low", "medium", "high")
PRI = {"high": 0, "medium": 1, "normal": 2, "low": 3}
CARGO_ENV = {"CARGO_BUILD_JOBS": WORKER_JOBS, "NEXTEST_TEST_THREADS": WORKER_TEST_THREADS, "CARGO_INCREMENTAL": "0", "CARGO_PROFILE_DEV_DEBUG": "line-tables-only"}


def e2e_port_for(wt):
    """This worker's own HK_E2E_PORT base: 9216 + 256 * (crc32(worktree name) % 146). A run at
    HK_E2E_CONCURRENCY=3 uses base .. base+216 (lanes base, +160, +192, each + a 24-port sweep) and
    canvas-journey its HK_E2E_JOURNEY_PORT = base+224 .. +252, so the 256 block holds both; >= 9216
    clears the gate's lanes even at 9 of them, and every block stays below macOS's ephemeral range
    (49152). Two of six workers share a block ~7 % of the time; backend.mjs's freePort steps past a
    taken port, as it did for any two runs before."""
    return 9216 + 256 * (zlib.crc32(os.path.basename(str(wt).rstrip("/")).encode()) % 146)


def e2e_env(wt):
    """The per-worker e2e environment: its own port block, and 3 lanes so the block holds the run."""
    base = e2e_port_for(wt)
    return {"HK_E2E_PORT": str(base), "HK_E2E_JOURNEY_PORT": str(base + 224), "HK_E2E_CONCURRENCY": "3"}


def log(msg):
    line = f"[{time.strftime('%m-%d %H:%M:%S')}] {msg}"
    print(line, file=sys.stderr)
    with open(LOG, "a") as f:
        f.write(line + "\n")


def attention(ticket, branch, kind, detail=""):
    with open(NEEDS, "a") as f:
        f.write(f"{time.strftime('%m-%d %H:%M')}  {branch}  {ticket}  {kind}  {detail}\n")
    log(f"ATTENTION {ticket} {kind} {detail}")
    # Under pytest the record above is all a test may produce: never alert or type into a live
    # tmux pane from a test (2026-09-23: test_work_accounting's fake CONFLICT_ESCALATE lines were
    # being send-keys'd, with Enter, into the coordinator's session on every test-py run).
    if os.environ.get("PYTEST_CURRENT_TEST"):
        return
    # Discord (user, 2026-09-23): the kinds a person must act on are alerts too. ops/alert.py
    # dedupes per key and never raises; NO_WORK / UNCOMMITTED are the coordinator's routine and
    # stay in the file only.
    level = {"BOARD_UNREADABLE": "red", "ERROR": "amber", "BLOCKED": "amber", "REVIEW_FAIL": "amber", "FIX_HELD": "info",
             "DEFLAKE_BLOCKED": "amber", "DEFLAKE_ERROR": "amber", "DEFLAKE_REVIEW_FAIL": "amber",
             "DEFLAKE_GATE_FAIL": "amber", "DEFLAKE_CONFLICT": "amber"}.get(kind)
    # "needs a person" only where no automation will pick it up (user, 2026-09-24 11:40): fix runs spent
    # or impossible, a cancellation to confirm, a review FAIL no fix round will take (REVIEW_FAIL is only
    # written then), and a BLOCKED hand-back that asks the user to decide.
    person = kind in ("CONFLICT_ESCALATE", "GATE_FAIL_ESCALATE", "CONFLICT_NO_SESSION", "GATE_FAIL_NO_SESSION",
                      "CANCEL_PROPOSED", "REVIEW_FAIL") or (kind == "BLOCKED" and re.search(r"\buser\b|decision", detail, re.I))
    if person:
        level = level or "amber"
    if level:
        try:
            subprocess.run([sys.executable, os.path.join(REPO, "ops", "alert.py"),
                            level, f"{'needs a person - ' if person else ''}{ticket} {kind}", f"{branch}: {detail[:300]}", "--key", f"wr:{ticket}:{kind}"],
                           capture_output=True, timeout=30)
        except Exception:
            pass
    # Poke the coordinator's pane the way the merge runner does; the file is the record, this is the wake-up.
    try:
        if subprocess.run(["tmux", "has-session", "-t", "dev"], capture_output=True).returncode == 0:
            subprocess.run(["tmux", "send-keys", "-t", "dev", "-l", f"WORK-RUNNER: {ticket} {kind} - {detail[:160]} See {NEEDS}."], capture_output=True)
            subprocess.run(["tmux", "send-keys", "-t", "dev", "Enter"], capture_output=True)
        else:   # incident 2026-09-24 04:07: the pane was gone 5.5 h and this returned quietly
            subprocess.run([sys.executable, os.path.join(REPO, "ops", "alert.py"), "--no-receiver", "dev",
                            f"WORK-RUNNER: {ticket} {kind} - {detail[:160]}"], capture_output=True, timeout=30)
    except Exception:
        pass


def sh(args, cwd=REPO, timeout=120, check=False):
    r = subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=timeout)
    if check and r.returncode != 0:
        raise RuntimeError(f"{' '.join(args)}: {r.stderr.strip()[:300]}")
    return r.stdout


# ---------- board ----------
def board():
    """Tickets from main's committed board, never the working tree (which the merge runner owns)."""
    text = sh(["git", "show", "main:docs/tasks.yaml"])
    return yaml.safe_load(text)["tasks"]


def deps_of(t):
    return list(t.get("depends_on") or t.get("deps") or [])


def is_user(t):
    fb = str(t.get("found_by", ""))[:40].lower()
    return bool(t.get("requested_by") or t.get("user_report") or fb.startswith("user"))


def ticket_num(tid):
    m = re.search(r"(\d+)", tid)
    return int(m.group(1)) if m else 10**9


def branch_of(tid):
    return "task-" + tid.lower().replace("-", "")


def worktree_of(tid):
    return f"{REPO}/.claude/worktrees/{tid.lower().replace('-', '')}"


def needs_review(t):
    return str(t.get("core_interface", "")).lower() == "true" or (t.get("model") or "sonnet") in ("sonnet", "haiku")


# ---------- claims ----------
def load_claims():
    try:
        return json.load(open(CLAIMS))
    except Exception:
        return {}


def save_claims(c):
    tmp = CLAIMS + ".tmp"
    json.dump(c, open(tmp, "w"), indent=1)
    os.replace(tmp, CLAIMS)


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


# ---------- environment guards ----------
def disk_free_gb():
    """`df`'s number, not statvfs: on APFS they differ by the purgeable space (12 GB on 2026-09-22),
    and CLAUDE.md's floor is stated in df terms ("only df counts")."""
    try:
        out = subprocess.run(["df", "-k", REPO], capture_output=True, text=True, timeout=10).stdout.splitlines()
        return int(out[-1].split()[3]) * 1024 / 1e9
    except Exception:
        st = os.statvfs(REPO)
        return st.f_bavail * st.f_frsize / 1e9


def gate_running():
    # `gate-wanted` is the merge runner waiting for the running workers to drain so the gate can
    # run alone: to dispatch it is the same as a gate in progress, or the drain never completes.
    return (os.path.exists(BULKMARK) or os.path.exists(f"{REPO}/.git/MERGE_HEAD")
            or os.path.exists(f"{S}/gate-wanted"))


# THE GATE SHARES THE BOX AGAIN (user, 2026-09-23 13:30). "The gate runs alone" (2026-09-22) was
# a crisis rule: it stopped the load flakes while their causes were unknown, at the price of
# serialising the box - gate (45 min, no dispatch) -> a minutes-wide dispatch window -> drain
# (up to 45 min, no gate). Measured 2026-09-23: dispatch was ZERO in 10 of 13 hours while the
# gate held the box 40-60 min of each, and the burndown went flat at ~1 ticket/hour once the
# crisis backlog had drained. The three flake causes are fixed at the root (a hidden tab's
# stopped rAF, a shared spec port, a self-matching wait loop), so the design this runner was
# built for is back: workers dispatch DURING a gate, capped at the gate's reserve
# ((CORES - GATE_RESERVE) / WORKER_CORES = 4), and the merge runner no longer waits for them
# (WORKER_DRAIN_MAX=0). WORK_GATE_ALONE=1 restores the crisis rule wholesale if it is ever
# needed again; nothing else changes with it.
GATE_ALONE = os.environ.get("WORK_GATE_ALONE", "0") == "1"
RESERVE_CAP = max(1, (CORES - GATE_RESERVE) // WORKER_CORES)


def gate_holds_dispatch():
    """True only in alone mode: a gate running or wanted stops every dispatch, fix runs included."""
    return GATE_ALONE and (os.path.exists(f"{S}/gate-wanted") or gate_running())


def queue_depth():
    """Branches waiting in merge-queue.txt (non-comment, non-blank lines)."""
    try:
        return sum(1 for l in open(f"{S}/merge-queue.txt") if l.strip() and not l.lstrip().startswith("#"))
    except OSError:
        return 0


def main_safe_to_commit():
    if gate_running():
        return False
    if sh(["git", "branch", "--show-current"]).strip() != "main":
        return False
    dirty = [l for l in sh(["git", "status", "--porcelain"]).splitlines() if not l.startswith("??")]
    return not dirty


def landed_tickets():
    out = {}
    try:
        for line in open(LANDED):
            try:
                d = json.loads(line)
                out[d.get("ticket", "")] = d.get("merge", "")
            except Exception:
                pass
    except FileNotFoundError:
        pass
    return out


# ---------- launch ----------
def bounded(cmd, cores=None):
    """Wrap a command in the worker bound - cpulimit (HiGarfield fork, descendants included), the
    background QoS clamp, nice - so EVERY agent this runner starts is bounded the same way: workers,
    reviewers and fix/resume runs alike. `cores` defaults to WORKER_CORES."""
    cores = cores or WORKER_CORES
    prefix = [CPULIMIT, "-l", str(cores * 100), "-i", "--"] if os.path.exists(CPULIMIT) else []
    if not prefix and not getattr(bounded, "_warned", False):
        log(f"NOTE: no cpulimit at {CPULIMIT} (see ops/README.md to build the fork) - QoS + env limits only"); bounded._warned = True
    return prefix + ["taskpolicy", "-c", "background", "nice", "-n", "10"] + cmd


def brief_for(t, wt, branch):
    d = f"{WORKDIR}/{t['id']}"          # where handback.json goes
    fields = {k: t.get(k) for k in ("id", "milestone", "title", "priority", "model", "effort", "core_interface",
                                     "parallel_group", "depends_on", "use_cases", "capabilities", "acceptance",
                                     "notes", "found_by", "requested_by") if t.get(k) is not None}
    body = yaml.safe_dump(fields, sort_keys=False, width=100, allow_unicode=True)
    return f"""You are working ticket {t['id']} for hackriff. This brief was assembled by ops/work-runner.py; there is no
coordinator in the loop, so read it fully and finish without asking questions.

WHERE: your worktree is {wt} on branch {branch}, cut from main. Work ONLY there. Never touch
/Users/daniellewis/hackriff (the main checkout), never `git stash`, never `git reset --hard`, never commit to main,
never append to any merge queue - the runner does that after you hand back.

DONE MEANS: the acceptance below is met, targeted tests pass, `just precheck <crates you touched>` is clean, and
everything is COMMITTED on {branch} with a message that starts "{t['id']}: " and ends with the line
Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
You do NOT edit docs/tasks.yaml (a hook denies it) and you do not need to: the runner writes the ticket's
result and status FROM YOUR HAND-BACK FILE (below). `just task show {t['id']}` prints the ticket if you need it.

TESTING PROTOCOL (CLAUDE.md): targeted tests only - `just test-crate <crate>`, `cargo nextest run -p <crate>
-E 'binary(<name>)'`, `just test-ui`. NEVER `just gate`, `just acceptance` or the full suite (a hook blocks
them). Never end a turn waiting on a background command; block on its output file instead.

READING ADRs (T-620): read the capability cards and ADRs the ticket names, but an ADR over ~300 lines
(0011-0013, 0015-0017, 0021, 0022: 11-28k tokens each) is read BY SECTION - where the ticket cites one
("ADR-0016 section 6"), read that; otherwise list them with `grep -n '^##' docs/adr/<file>` and read only
the ones your change touches. Never read docs/01-05 end to end.

FILING RULE (user, 2026-09-22): do not file new tickets for things you merely suspect. An OBSERVED failure
you cannot fix in scope goes in your result: text with the exact evidence; the coordinator decides.

HAND BACK: your LAST step is to write this file, exactly this shape (JSON, no comments):
  {d}/handback.json
  {{"ticket": "{t['id']}",
   "outcome": "done" | "blocked" | "cancel",
   "summary": "what changed and why - 3 to 10 lines, written for the ticket's result: field",
   "commits": ["<short sha>", ...],
   "files": ["<path>", ...],
   "tests": [{{"cmd": "just test-crate hk-x", "exit": 0, "summary": "41 passed"}}, ...],
   "precheck": {{"exit": 0}},
   "blocked": {{"needs": "<what specifically, if outcome is blocked>"}},
   "cancel": {{"evidence": "<why this ticket needs NO work: done by T-x at <sha>, or obsoleted by <decision>>"}},
   "observed_but_not_chased": ["<an observed failure outside scope, with the exact evidence>", ...],
   "use_cases": ["<the use-case ids your tests assert on>", ...]}}
The runner validates it, writes the ticket's result from it on your branch, routes on `outcome`, and refuses
"done" if any test exit is non-zero - unless that red is not yours: mark it "known_flake": true or
"reproduces_on_main": true (and say how you know in its summary) and the branch still queues; the gate decides. A deliberate red
proof (your new test on the old code, or the defect re-injected) is marked "expect": "red" and counts only beside a green run. A CANCEL is yours to propose with evidence in the repo; an Opus review
confirms it before it lands. Also end your final message with one line `HANDBACK: <outcome>` as a fallback.
Never exit with no commits and no hand-back file - that reads as a lost agent, not a finding.

TICKET:
{body}
"""


def clone_cmd(wt):
    """The shell that seeds a new worktree's target/ as an APFS clone of main's - or nothing when
    WORK_CLONE_TARGET=0 (the worker then builds from sccache). Each clone is a pin that turns exclusive as
    gates rebuild main's target/ (2026-09-24 18:11: 83 GB still shared across three workers, 101 GB free)."""
    if not CLONE_TARGET:
        return ""
    return f'[ -d "{REPO}/target" ] && [ ! -e "{wt}/target" ] && cp -c -R -p "{REPO}/target" "{wt}/target"; '


# ---------- remote hosts (user, 2026-09-24 23:35: worker agents on a second computer) ----------
# A remote host only WORKS - builds, targeted tests, hands back; the Mac alone gates and merges. The host has its own
# roots (hosts.json `repo`, `ops`); every command, file and path that crosses ssh is rewritten from this Mac's REPO and
# $HACKRIFF_OPS to them at the boundary (to_remote), so the rest of this runner keeps using its own paths.
# The claim's pid is a plain local `ssh` running the remote wrapper: while it lives the run lives, and its
# stdout/stderr stream into this Mac's out.json/run.log as for a local worker. The wrapper also records the remote
# process group ({d}/remote.pgid) and tees both streams into the box's copy of the work dir, so a dropped
# connection loses nothing: the reap asks the box whether that group still runs, re-attaches if it does, and
# copies the complete files back when it ends. Every stop and every resume stops that group EXPLICITLY first.
# {"node2": {"ssh": "ubuntu@10.198.1.109", "repo": "/home/ubuntu/hk/hackriff", "ops": "/home/ubuntu/hk/ops",
#            "env": {"CHROME": "/snap/bin/chromium"}}} - the name is also this Mac's git remote for the host's mirror.
HOSTS_FILE = f"{S}/hosts.json"      # absent = no remote host
SSH_OPTS = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=30", "-o", "ServerAliveCountMax=6"]
_REMOTE_PATH = 'export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"; '


def hosts():
    try:
        return json.load(open(HOSTS_FILE))
    except (OSError, ValueError):
        return {}


def host_for(t):
    """Stage 1: a ticket goes remote only when named by hand (WORK_REMOTE_TICKETS=T-nnn,...)."""
    named = {x.strip() for x in os.environ.get("WORK_REMOTE_TICKETS", "").split(",") if x.strip()}
    h = hosts()
    return next(iter(h), None) if h and t.get("id") in named else None


def to_remote(host, text):
    """This Mac's paths in `text` -> the host's (its ops dir, its clone)."""
    h = hosts()[host]
    return text.replace(S, h["ops"]).replace(REPO, h["repo"])


def ssh_argv(host, remote_cmd):
    # Explicit bash: the wrapper's `>(...)` is bash syntax, whatever the account's login shell is.
    return ["ssh", *SSH_OPTS, hosts()[host]["ssh"], "bash -c " + shlex.quote(_REMOTE_PATH + to_remote(host, remote_cmd))]


def remote_sh(host, remote_cmd, timeout=120, input=None):
    """(returncode, stdout) of a command on the host; 255 = ssh itself failed (unreachable, key refused)."""
    try:
        r = subprocess.run(ssh_argv(host, remote_cmd), input=input, capture_output=True, text=True, timeout=timeout)
        return r.returncode, r.stdout
    except (subprocess.TimeoutExpired, KeyError) as e:
        return 255, str(e)


def remote_wrapper(wt, branch, script, env, d, out_name):
    """The remote side of a run: the worker in its own session, its group id and both streams recorded on the box;
    the branch pushed to the mirror when it ends, whatever its outcome (the Mac fetches it before the reap)."""
    q = shlex.quote
    envs = " ".join(f"{k}={q(str(v))}" for k, v in env.items())
    inner = f"cd {q(wt)} && exec env {envs} nice -n 5 bash -c {q(script)}"
    return (f"mkdir -p {q(d)}; "
            # tee -p: a dropped connection (SIGPIPE on the channel) must not kill the copy on the host, nor the worker.
            f"setsid bash -c {q(inner)} </dev/null > >(tee -p {q(d + '/' + out_name)}) 2> >(tee -p -a {q(d + '/run.log')} >&2) & p=$!; "
            f"echo $p > {q(d + '/remote.pgid')}; wait $p; rc=$?; rm -f {q(d + '/remote.pgid')}; "
            f"git -C {q(wt)} push -q --no-verify -f origin HEAD:refs/heads/{branch} >&2; exit $rc")


def remote_run_state(c):
    """'running' | 'gone' | 'unknown' (the host could not be asked) for a remote claim's recorded process group."""
    d = f"{WORKDIR}/{c['ticket']}"
    rc, out = remote_sh(c["host"], f"pg=$(cat {shlex.quote(d + '/remote.pgid')} 2>/dev/null) || {{ echo gone; exit 0; }}; "
                                   f"kill -0 -- -$pg 2>/dev/null && echo running || echo gone", timeout=60)
    return out.strip() if rc == 0 and out.strip() in ("running", "gone") else "unknown"


def remote_stop(c, wait_s=30):
    """Stop the claim's remote process group and confirm it is gone; False when that cannot be confirmed."""
    d = f"{WORKDIR}/{c['ticket']}"
    rc, out = remote_sh(c["host"], f"pg=$(cat {shlex.quote(d + '/remote.pgid')} 2>/dev/null) || {{ echo gone; exit 0; }}; "
                                   f"kill -TERM -- -$pg 2>/dev/null; for i in $(seq {wait_s}); do kill -0 -- -$pg 2>/dev/null || {{ echo gone; exit 0; }}; sleep 1; done; "
                                   f"kill -KILL -- -$pg 2>/dev/null; sleep 1; kill -0 -- -$pg 2>/dev/null && echo running || echo gone", timeout=wait_s + 60)
    return rc == 0 and out.strip().endswith("gone")


def remote_prepare(host, wt, branch, d, resume=False):
    """Mirror the gated base to the box as its `main` (fix prompts merge it) and copy LFS objects. The worktree is made
    only when it does not exist, and never reset: on a resume (and on a re-dispatch) the host's copy - its commits and
    its uncommitted files - is the newer one, and this Mac's branch is pushed only when the mirror has none."""
    base = merge_target()
    # --no-verify: the only pre-push hook is Git LFS's upload, which the mirror cannot serve - LFS objects go by rsync.
    sh(["git", "push", "-q", "--no-verify", "-f", host, f"{base}:refs/heads/main"], check=True)
    if not resume and sh(["git", "rev-parse", "--verify", "-q", branch]).strip():
        sh(["git", "push", "-q", "--no-verify", host, f"{branch}:refs/heads/{branch}"])      # no -f: never over the host's
    sh(["rsync", "-a", "-e", "ssh " + " ".join(SSH_OPTS), f"{REPO}/.git/lfs/objects/",
        f"{hosts()[host]['ssh']}:{to_remote(host, REPO)}/.git/lfs/objects/"], timeout=600, check=True)
    q = shlex.quote
    rc, out = remote_sh(host, f"set -e; cd {q(REPO)}; git fetch -q origin; git branch -f main origin/main; git worktree prune; mkdir -p {q(d)}; "
                              f"if [ ! -d {q(wt)} ]; then "
                              f"if git rev-parse -q --verify origin/{branch} >/dev/null; then git worktree add -q -B {branch} {q(wt)} origin/{branch}; "
                              f"else git worktree add -q -b {branch} {q(wt)} origin/main; fi; "
                              f"git -C {q(wt)} lfs checkout >/dev/null 2>&1 || true; fi", timeout=600)
    if rc:
        raise RuntimeError(f"remote_prepare on {host}: {out.strip()[-200:]}")


def remote_put(host, path, text):
    rc, out = remote_sh(host, f"mkdir -p {shlex.quote(os.path.dirname(path))} && cat > {shlex.quote(path)}", input=to_remote(host, text))
    if rc:
        raise RuntimeError(f"remote_put {host}:{path}: {out.strip()[:200]}")


def remote_popen(host, wt, branch, script, env, d, out_name, out, err):
    """The local end of a remote run: a plain ssh in its own session - the claim's pid."""
    env = dict(env, **hosts()[host].get("env", {}))
    return subprocess.Popen(ssh_argv(host, remote_wrapper(wt, branch, script, env, d, out_name)), stdin=subprocess.DEVNULL,
                            stdout=out, stderr=err, start_new_session=True)


def remote_attach(c):
    """The connection dropped but the remote run lives: a new local ssh that waits for its group - the claim's pid."""
    d = f"{WORKDIR}/{c['ticket']}"
    p = subprocess.Popen(ssh_argv(c["host"], f"pg=$(cat {shlex.quote(d + '/remote.pgid')}); while kill -0 -- -$pg 2>/dev/null; do sleep 10; done"),
                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    return p.pid


def sync_back(c):
    """A remote run ended: its complete out file and run.log, its handback.json, its branch into this Mac's worktree,
    and the box's uncommitted files (for the reap's UNCOMMITTED rule). False = could not; the reap waits a tick."""
    host, tid, branch, wt = c["host"], c["ticket"], c["branch"], c["wt"]
    d = f"{WORKDIR}/{tid}"
    try:
        dest = hosts()[host]["ssh"]
        out_name = os.path.basename(c.get("out") or f"{d}/out.json")
        for f in (out_name, "run.log", "handback.json"):
            r = subprocess.run(["scp", *SSH_OPTS, "-q", f"{dest}:{to_remote(host, d)}/{f}", f"{d}/{f}.remote"], capture_output=True, timeout=120)
            if r.returncode == 0:
                os.replace(f"{d}/{f}.remote", f"{d}/{f}")
            elif f != "handback.json":
                raise RuntimeError(f"scp {f}: {r.stderr.decode(errors='replace').strip()[:160]}")
            elif os.path.exists(f"{d}/handback.json"):
                os.remove(f"{d}/handback.json")          # never judge this run by an older hand-back
        sh(["git", "fetch", "-q", host, f"+refs/heads/{branch}:refs/remotes/{host}/{branch}"], check=True, timeout=300)
        if os.path.isdir(wt):
            sh(["git", "reset", "-q", "--hard", f"{host}/{branch}"], cwd=wt, check=True)
        rc, dirty = remote_sh(host, f"git -C {shlex.quote(wt)} status --porcelain --untracked-files=no", timeout=60)
        if rc:
            raise RuntimeError(f"remote status: {dirty.strip()[:160]}")
        c["remote_dirty"] = [l for l in dirty.splitlines() if l.strip()]
        log(f"REMOTE {tid}: synced back from {host} at {sh(['git', 'rev-parse', '--short', host + '/' + branch]).strip()}"
            + (f"; {len(c['remote_dirty'])} file(s) left uncommitted there" if c["remote_dirty"] else ""))
        return True
    except Exception as e:
        log(f"REMOTE {tid}: sync back from {host} FAILED ({e}) - held; retried next tick")
        if not c.get("sync_warned"):
            c["sync_warned"] = True
            attention(tid, branch, "REMOTE_SYNC", f"could not bring the run back from {host}: {str(e)[:200]}")
        return False


def launch(t, dry):
    tid, branch, wt = t["id"], branch_of(t["id"]), worktree_of(t["id"])
    model = MODEL_ALIAS.get((t.get("model") or "sonnet").lower(), "sonnet")
    effort = (t.get("effort") or "medium").lower()
    if dry:
        log(f"DRY-RUN would dispatch {tid} [{model}/{effort}] group={t.get('parallel_group')} -> {branch}")
        return None
    if sh(["git", "rev-parse", "--verify", "-q", branch]).strip():
        if os.path.isdir(wt):
            log(f"REUSE {tid}: branch and worktree exist with no commits - a dispatch that never ran")
        else:
            sh(["git", "worktree", "add", wt, branch], check=True)
    else:
        sh(["git", "worktree", "add", wt, "-b", branch, "main"], check=True)
    d = f"{WORKDIR}/{tid}"
    os.makedirs(d, exist_ok=True)
    brief = brief_for(t, wt, branch)
    open(f"{d}/brief.md", "w").write(brief)
    # The session id is chosen here, not read from out.json at the end: a run killed by a signal
    # writes no out.json, and without the id it could never be resumed (incident 2026-09-24 04:07).
    session = str(uuid.uuid4())
    cmd = ["claude", "-p", "--agent", "worker", "--model", model, "--dangerously-skip-permissions",
           "--output-format", "json", "--max-budget-usd", BUDGET_USD, "--session-id", session]
    if effort in EFFORTS:
        cmd += ["--effort", effort]
    # The build-target clone (`cp -c`, an APFS clone) walks main's whole target tree and takes
    # minutes; done inline it blocked every tick for that long (2026-09-22: two dispatches took
    # eight minutes of a tick). So the clone runs INSIDE the worker's own process, which then
    # `exec`s claude under the same pid - the claim's pid is valid from the first second, reap sees
    # it alive through both phases, and the tick returns at once. The brief is read from its file.
    host = host_for(t)
    if host:
        try:
            remote_prepare(host, wt, branch, d)
            remote_put(host, f"{d}/brief.md", brief)
        except Exception as e:
            log(f"REMOTE {tid}: could not prepare {host} ({e}) - not dispatched this tick")
            return None
        script = "exec " + " ".join(f"'{a}'" for a in cmd) + f" < '{d}/brief.md'"
        env = dict(CARGO_ENV, HK_WORKER="1", HACKRIFF_OPS=S, **e2e_env(wt))
        p = remote_popen(host, wt, branch, script, env, d, "out.json", open(f"{d}/out.json", "w"), open(f"{d}/run.log", "a"))
        log(f"DISPATCH {tid} [{model}/{effort}] pid={p.pid} -> {host}:{wt} (remote; this Mac holds the ssh session)")
        return {"ticket": tid, "branch": branch, "wt": wt, "pid": p.pid, "started": time.time(), "model": model,
                "effort": effort, "group": t.get("parallel_group"), "milestone": t.get("milestone"), "kind": "work",
                "review": needs_review(t), "session_id": session, "host": host}
    clone = clone_cmd(wt)
    script = clone + "exec " + " ".join(f"'{a}'" for a in cmd) + f" < '{d}/brief.md'"
    env = dict(os.environ, **CARGO_ENV, HK_WORKER="1", HACKRIFF_OPS=S, **e2e_env(wt))
    out = open(f"{d}/out.json", "w")
    err = open(f"{d}/run.log", "a")
    # The bound, inherited by the whole tree, three layers: (1) CPULIMIT - the HiGarfield fork of
    # cpulimit, built from source into $HACKRIFF_OPS/bin (Homebrew's opsengine build is INERT on
    # Apple Silicon: measured 0 % effect; the fork, `-l 200 -i` over four busy loops, measured 164 %
    # of CPU in aggregate - a real ceiling by SIGSTOP/SIGCONT, descendants included); (2) a permanent
    # `background` QoS clamp (efficiency cores only, low priority); (3) CARGO/NEXTEST limits in env
    # so the build and test runners never ask for more. Without the fork binary, layers 2-3 still hold.
    cmd_prefix = ["cpulimit"] if os.path.exists(CPULIMIT) else []
    p = subprocess.Popen(bounded(["bash", "-c", script]), cwd=wt,
                         stdin=subprocess.DEVNULL, stdout=out, stderr=err, env=env, start_new_session=True)
    log(f"DISPATCH {tid} [{model}/{effort}] pid={p.pid} -> {wt} (target clone then exec claude; {'cpulimit ' + str(WORKER_CORES * 100) + '% + ' if cmd_prefix else ''}background QoS, jobs={WORKER_JOBS}, test-threads={WORKER_TEST_THREADS})")
    return {"ticket": tid, "branch": branch, "wt": wt, "pid": p.pid, "started": time.time(), "model": model,
            "effort": effort, "group": t.get("parallel_group"), "milestone": t.get("milestone"), "kind": "work",
            "review": needs_review(t), "session_id": session}


def launch_review(claim):
    tid, branch, wt = claim["ticket"], claim["branch"], claim["wt"]
    d = claim.get("dir") or f"{WORKDIR}/{tid}"
    cancel = c_reason = claim.get("cancel_reason")
    extra = (f"\nTHIS BRANCH CANCELS THE TICKET. The worker's reason: {cancel}\nYour job is to verify that reason against the repo "
             "(is the work really done at the commit named? is the decision real and does it obsolete THIS ticket?). "
             "PASS only if the evidence holds; FAIL names what is missing.\n") if cancel else ""
    if claim.get("deflake"):
        extra += (f"\nTHIS BRANCH IS A DEFLAKE RUN for the flaky test {claim.get('test')}, not a ticket (its brief is the "
                  "'ticket text'). FAIL it if the fix masks rather than removes the nondeterminism: a retry, a skip, an "
                  "#[ignore], a quarantine entry, a widened or shortened timeout, or a deleted/weakened assertion. PASS needs "
                  "the cause named, a deterministic fix, and the hand-back's proof that the test goes red when the defect returns.\n")
    prompt = f"""Review branch {branch} for hackriff before it is queued for merge. The diff is `git diff main...{branch}`{extra}
(run it from {wt}). The ticket text is in {d}/brief.md, the worker's structured hand-back in {d}/handback.json
(review the diff against what it CLAIMS: tests listed, files listed, summary) and its transcript result in {d}/out.json.
Check what the reviewer agent definition says to check, with CLAUDE.md's invariants and the thin-client rule.
Do not edit anything. Your final message must end with exactly one line:
VERDICT: PASS
VERDICT: FAIL <one line naming the defect and the file:line>
"""
    cmd = ["claude", "-p", "--agent", "reviewer", "--model", "opus", "--dangerously-skip-permissions",
           "--output-format", "json", "--max-budget-usd", BUDGET_USD]
    out = open(f"{d}/review.json", "w")
    err = open(f"{d}/run.log", "a")
    p = subprocess.Popen(bounded(cmd), cwd=wt, stdin=subprocess.PIPE, stdout=out, stderr=err,
                         env=dict(os.environ, **CARGO_ENV, HACKRIFF_OPS=S), start_new_session=True, text=True)
    p.stdin.write(prompt)
    p.stdin.close()
    log(f"REVIEW {tid} pid={p.pid} (bounded)")
    return dict(claim, pid=p.pid, started=time.time(), kind="review")


# ---------- per-run resource accounting ----------
# What did this ticket actually COST the box? Until now: nothing recorded. `work-done.jsonl` had
# minutes, dollars and turns, which say what the model spent, not what the machine did - so
# "which tickets are expensive to build" and "is the 3-core bound holding" were both unanswerable,
# and on 2026-09-22 a leaked process tree was invisible until someone ran `ps`.
#
# CPU time cannot be read at reap: by then the root process has exited and the kernel has thrown
# its accounting away. So it is SAMPLED each tick and the running maximum is kept. That
# undercounts - a rustc that starts and finishes between two ticks contributes nothing - so these
# are a floor on the cost, not a measurement of it, and the result line says CPU-s rather than
# pretending to be exact.
def _cpu_seconds(s):
    """`ps -o time` -> seconds. macOS prints `MM:SS.ss`, or `HH:MM:SS.ss` past an hour."""
    try:
        parts = s.strip().split(":")
        return int(parts[0]) * 60 + float(parts[1]) if len(parts) == 2 else \
            int(parts[0]) * 3600 + int(parts[1]) * 60 + float(parts[2])
    except Exception:
        return 0.0


def _ps_tree_rows():
    try:
        out = subprocess.run(["ps", "-axo", "pid=,ppid=,pgid=,rss=,time=,command="],
                             capture_output=True, text=True, timeout=20).stdout
    except Exception:
        return []
    rows = []
    for line in out.splitlines():
        f = line.split(None, 5)
        if len(f) < 6:
            continue
        try:
            rows.append({"pid": int(f[0]), "ppid": int(f[1]), "pgid": int(f[2]),
                         "rss_mb": int(f[3]) / 1024.0, "cpu_s": _cpu_seconds(f[4]), "cmd": f[5]})
        except ValueError:
            continue
    return rows


def sample_group(pid, rows=None):
    """(cpu_s, rss_mb, member_pids, member_pgids) for a claim's whole tree.

    NOT just `ps -g <pgid>`: measured 2026-09-23, a claim's process group contains only the
    `cpulimit` wrapper itself - `claude` and everything it spawns get their own groups. So the
    tree is the pid, anything in its group, and every descendant, walked transitively.
    """
    rows = rows if rows is not None else _ps_tree_rows()
    by = {r["pid"]: r for r in rows}
    kids = {}
    groups = {}
    for r in rows:
        kids.setdefault(r["ppid"], []).append(r["pid"])
        groups.setdefault(r["pgid"], []).append(r["pid"])
    seen, stack = set(), [pid]
    while stack:
        p = stack.pop()
        if p in seen or p not in by:
            continue
        seen.add(p)
        stack.extend(kids.get(p, []))
        stack.extend(groups.get(p, []))
    return (round(sum(by[p]["cpu_s"] for p in seen), 1),
            round(sum(by[p]["rss_mb"] for p in seen), 1),
            sorted(seen), sorted({by[p]["pgid"] for p in seen}))


def track_usage(c):
    """Fold this tick's sample into the claim; True when it changed anything.

    The return value matters: the caller uses it to mark the claims file dirty. Without that the
    sample lives only in memory and is lost at the next restart — and the whole point is that the
    numbers survive the run they describe. Peaks only, because the high-water mark is the
    question (`c["cpu_s"]` is monotonic by construction, so this is effectively every tick).
    """
    try:
        cpu, rss, pids, pgids = sample_group(c["pid"])
    except Exception:
        return False
    before = (c.get("cpu_s"), c.get("peak_rss_mb"), c.get("tree_pids"))
    c["cpu_s"] = max(c.get("cpu_s") or 0, cpu)
    c["peak_rss_mb"] = max(c.get("peak_rss_mb") or 0, rss)
    if pids:
        c["tree_pids"], c["tree_pgids"] = pids[:200], pgids[:50]
    return before != (c.get("cpu_s"), c.get("peak_rss_mb"), c.get("tree_pids"))


def leaked_processes(c, rows=None):
    """Processes of this run still alive after its root exited.

    Ancestry cannot find them - a leaked process is reparented to launchd, which is the whole
    problem - so they are recognised two ways, and a pid must match one of them: it is a pid (in
    a process group) this run was seen holding, or its command line names this run's WORKTREE.
    The worktree path is the strong one: every cargo, rustc and test binary of this ticket
    carries it, and no other run's does. Both guards exist because pids are recycled, and a
    SIGKILL aimed at a recycled pid is a far worse bug than a leaked process.
    """
    rows = rows if rows is not None else _ps_tree_rows()
    pids, pgids, wt = set(c.get("tree_pids") or []), set(c.get("tree_pgids") or []), c.get("wt") or ""
    out = []
    for r in rows:
        if r["pid"] <= 1:
            continue
        if (r["pid"] in pids and r["pgid"] in pgids) or (wt and wt in r["cmd"]):
            out.append(r)
    return out


def kill_leaked(rows):
    """SIGTERM, ten seconds, SIGKILL. A cargo or nextest given a chance to exit cleanly leaves a
    usable target dir behind; one that is SIGKILLed mid-write does not."""
    for r in rows:
        try:
            os.kill(r["pid"], signal.SIGTERM)
        except OSError:
            pass
    time.sleep(10)
    killed = []
    for r in rows:
        try:
            os.kill(r["pid"], 0)
            os.kill(r["pid"], signal.SIGKILL)
            killed.append(r["pid"])
        except OSError:
            pass
    return killed


def resource_line(c):
    """The one line a person reads: what this ticket cost the box, and what it left behind."""
    if not c.get("cpu_s") and not c.get("peak_rss_mb"):
        return ""
    s = f"Resources: {c.get('cpu_s', 0):.0f} CPU-s, peak {(c.get('peak_rss_mb') or 0) / 1024:.1f} GB"
    if c.get("leaked"):
        s += f" — LEAKED {c['leaked']} processes (killed at reap)"
    return s


# ---------- reap ----------
def result_of(path):
    try:
        d = json.load(open(path))
        if isinstance(d, list):
            d = next((x for x in reversed(d) if x.get("type") == "result"), d[-1] if d else {})
        return d
    except Exception:
        return {}


def load_handback(d, tid):
    """The worker's hand-back file: the contract. (dict, None) when valid; (None, reason) otherwise."""
    p = f"{d}/handback.json"
    if not os.path.exists(p):
        return None, "no handback.json"
    try:
        hb = json.load(open(p))
    except Exception as e:
        return None, f"handback.json is not JSON: {str(e)[:80]}"
    if not isinstance(hb, dict) or hb.get("outcome") not in ("done", "blocked", "cancel"):
        return None, "handback.json: outcome must be done|blocked|cancel"
    if str(hb.get("ticket", tid)) != tid:
        return None, f"handback.json names {hb.get('ticket')}, not {tid}"
    if not isinstance(hb.get("summary", ""), str) or not hb.get("summary", "").strip():
        return None, "handback.json: summary missing"
    return hb, None


def handback_outcome(hb, text):
    """(outcome, why) from the JSON when valid, else from the fallback HANDBACK: line, else done-by-default
    (the commit/dirty checks that follow still decide what actually happens)."""
    if hb:
        o = hb["outcome"]
        why = (hb.get("blocked") or {}).get("needs") if o == "blocked" else (hb.get("cancel") or {}).get("evidence") if o == "cancel" else ""
        return o, why or ""
    line = next((l for l in text.splitlines() if l.startswith("HANDBACK:")), "")
    if "BLOCKED" in line:
        return "blocked", line[18:300]
    if "CANCEL" in line:
        return "cancel", line[17:300]
    return "done", ""


def write_result(c, hb):
    """Write the ticket's result: block on the worker's branch from handback.json, through the task CLI,
    committed by the runner - so the board edit is deterministic and workers never touch tasks.yaml.
    Needs the CLI on the branch (py/hkpy/tasks.py, task-taskcli); otherwise the summary waits in the file."""
    tid, wt = c["ticket"], c["wt"]
    if not os.path.exists(f"{wt}/py/hkpy/tasks.py"):
        log(f"RESULT {tid}: no task CLI on the branch yet; summary stays in handback.json")
        return
    lines = [f"{hb['outcome'].upper()} (work-runner, from handback.json, {time.strftime('%Y-%m-%d %H:%M')}).", hb["summary"].strip()]
    if hb.get("tests"):
        lines.append("Tests: " + "; ".join(f"{t.get('cmd')} -> exit {t.get('exit')}{' (' + str(t.get('summary')) + ')' if t.get('summary') else ''}" for t in hb["tests"] if isinstance(t, dict)))
    if hb.get("precheck"):
        lines.append(f"precheck exit {hb['precheck'].get('exit')}")
    if hb.get("commits"):
        lines.append("Commits: " + ", ".join(map(str, hb["commits"])))
    if hb.get("observed_but_not_chased"):
        lines.append("Observed, not chased: " + " | ".join(map(str, hb["observed_but_not_chased"])))
    if hb.get("use_cases"):
        lines.append("Use cases: " + ", ".join(map(str, hb["use_cases"])))
    if (rl := resource_line(c)):
        lines.append(rl)
    if hb["outcome"] == "cancel":
        lines.append("Cancel evidence: " + str((hb.get("cancel") or {}).get("evidence", "")))
    rp = f"{WORKDIR}/{tid}/result.txt"
    open(rp, "w").write("\n".join(lines) + "\n")
    args = ["uv", "run", "--locked", "--project", "py", "python", "-m", "hkpy.tasks"]
    r = subprocess.run(args + ["result", tid, "--from", rp, "--file", f"{wt}/docs/tasks.yaml"], cwd=wt, capture_output=True, text=True)
    if r.returncode == 0 and hb["outcome"] == "cancel":
        r = subprocess.run(args + ["set", tid, "status=cancelled", f"cancelled_reason={(hb.get('cancel') or {}).get('evidence', '')[:300]}", "--file", f"{wt}/docs/tasks.yaml"], cwd=wt, capture_output=True, text=True)
    if r.returncode != 0:
        log(f"RESULT {tid}: task CLI failed: {(r.stderr or r.stdout).strip()[:160]}")
        subprocess.run(["git", "checkout", "--", "docs/tasks.yaml"], cwd=wt, capture_output=True)
        return
    subprocess.run(["git", "add", "docs/tasks.yaml"], cwd=wt, capture_output=True)
    r = subprocess.run(["git", "commit", "-q", "-m", f"{tid}: result and status from handback.json (work-runner)"], cwd=wt, capture_output=True, text=True)
    log(f"RESULT {tid}: board {'written on the branch' if r.returncode == 0 else 'commit failed: ' + r.stderr.strip()[:100]}")


def record_done(claim, outcome, res):
    # A fix run says WHY it ran (user, 2026-09-24) - the /worklog "Fix runs" table and the digest tally.
    why = ({"attempt": claim.get("fix_attempts"), "reason_class": claim.get("fix_reason_class") or "OTHER",
            "reason": claim.get("fix_reason") or ""} if claim.get("kind") == "fix" else {})
    with open(DONE, "a") as f:
        f.write(json.dumps({**why, "ticket": claim["ticket"], "branch": claim["branch"], "kind": claim.get("kind"),
                            "started": int(claim["started"]), "finished": int(time.time()),
                            "minutes": round((time.time() - claim["started"]) / 60, 1),
                            "cost_usd": res.get("total_cost_usd"), "turns": res.get("num_turns"),
                            "model": claim.get("model"), "outcome": outcome,
                            # What the BOX spent, beside what the model spent. Sampled per tick,
                            # so a floor rather than an exact total (see sample_group).
                            "cpu_s": claim.get("cpu_s"), "peak_rss_mb": claim.get("peak_rss_mb"),
                            "leaked": claim.get("leaked", 0)}) + "\n")


def enqueue(branch, wt=None):
    lines = [l.strip() for l in open(MERGE_QUEUE)] if os.path.exists(MERGE_QUEUE) else []
    if branch not in lines:
        with open(MERGE_QUEUE, "a") as f:
            f.write(branch + "\n")
    log(f"QUEUED {branch} for merge")
    # A worker's build output is real disk (not a clone) - ~4-8 GB each, and twenty of them emptied
    # a 45 GB margin in 20 minutes (2026-09-22). Once the branch is queued the target is dead weight;
    # the source tree stays so a gate-failure fix can resume and rebuild (sccache makes that cheap).
    if wt and os.path.isdir(os.path.join(wt, "target")):
        shutil.rmtree(os.path.join(wt, "target"), ignore_errors=True)
        log(f"RECLAIM {wt}/target (branch queued)")


def alert(level, title, body, key):
    """ops/alert.py (Discord, deduped per key); never raises."""
    try:
        subprocess.run([sys.executable, os.path.join(REPO, "ops", "alert.py"), level, title, body, "--key", key],
                       capture_output=True, timeout=30)
    except Exception:
        pass


_SPEC = re.compile(r"([a-z0-9-]+)(?:\.e2e\.mjs)?")


def not_own_red(t):
    """A failing listed test the worker marks as not its own ("known_flake" / "reproduces_on_main"), or whose
    browser specs are ALL ones the flake ledger has seen pass alone (hkpy.flakes) - not a branch defect."""
    if t.get("known_flake") or t.get("reproduces_on_main"):
        return True
    cmd = str(t.get("cmd", ""))
    if "run.mjs" not in cmd:
        return False
    specs = [m + ".e2e.mjs" for m in _SPEC.findall(cmd.split("run.mjs", 1)[1]) if m and not m.startswith("-")]
    if not specs:
        return False
    try:
        if f"{REPO}/py" not in sys.path:
            sys.path.append(f"{REPO}/py")
        from hkpy import flakes
        led = flakes.ledger(S)
    except Exception:
        return False
    return all(led.get(s) is not None and led[s].passed_alone > 0 for s in specs)


def hand_back_reds(hb):
    """(red, own): the hand-back's failing tests, and those of them that are the branch's own defect.
    A worker's red proof (its new test on the OLD code, or the defect re-injected) exits non-zero by design
    and says so with "expect": "red" - the deflaker's convention with the same guard: it counts only beside
    a green run. Three DONE hand-backs read BLOCKED 'needs a person' on exactly that in 24 h (T-894 15:25,
    T-905 20:16 on 2026-09-24; the app-trace deflaker at 01:32 before its own fix)."""
    tests = [t for t in (hb or {}).get("tests", []) if isinstance(t, dict)]
    green = any(int(t.get("exit", 0) or 0) == 0 for t in tests)
    red = [t for t in tests if int(t.get("exit", 0) or 0) != 0 and not (t.get("expect") == "red" and green)]
    return red, [t for t in red if not not_own_red(t)]


def reap(claims, dry):
    changed = False
    killed = []
    for tid, c in list(claims.items()):
        if c.get("state") != "running":
            continue
        pid = c["pid"]
        age_min = (time.time() - c["started"]) / 60
        limit = REVIEW_MAX_MINUTES if c["kind"] == "review" else MAX_MINUTES
        if alive(pid) and not c.get("detached"):
            if track_usage(c):                      # the only chance to see this run's CPU time
                changed = True
            if age_min > limit:
                try:
                    os.killpg(pid, signal.SIGTERM)
                except OSError:
                    pass
                if c.get("host"):
                    remote_stop(c)
                c["state"] = "timeout"
                c["ended"] = time.time()
                record_done(c, "timeout", {})
                changed = True
                if (c["kind"] == "work" and c.get("session_id") and c.get("timeout_resumes", 0) < TIMEOUT_RESUMES
                        and os.path.isdir(c.get("wt", "")) and _gone(pid)):
                    claims[tid] = launch_fix(dict(c, kind="work"), f"TIMEOUT your run reached the {limit}-min limit and was stopped")
                    log(f"TIMEOUT {tid}: resumed once to wrap up ({claims[tid].get('state')})")
                else:
                    attention(tid, c["branch"], "TIMEOUT", f"{c['kind']} exceeded {limit} min; killed; worktree kept")
            continue
        # finished - for a remote claim (a review runs on this Mac), the LOCAL ssh ended: ask the box first
        if c.get("host") and c["kind"] != "review":
            state = remote_run_state(c)
            if state == "running":
                c["pid"], c["detached"] = remote_attach(c), False
                log(f"REMOTE {tid}: connection to {c['host']} dropped, run still going there - re-attached (pid {c['pid']})")
                changed = True
                continue
            if state == "unknown" or not sync_back(c):
                if state == "unknown" and not c.get("detached"):
                    log(f"REMOTE {tid}: {c['host']} unreachable - the claim waits, polled on the host (stage 4 re-dispatches)")
                    c["detached"] = True           # its local pid is dead: never signal it again (pids recycle)
                    changed = True
                continue
            c["detached"] = False
        changed = True
        # The root is gone; anything of this run still running is a LEAK, holding cores and disk
        # for work nobody is waiting for. Nothing used to notice - a killed session's cargo could
        # run for hours beside the gate, which is the 2026-09-22 contention in another form.
        try:
            leaked = leaked_processes(c)
            if leaked:
                c["leaked"] = len(leaked)
                log(f"LEAKED {tid}: {len(leaked)} process(es) outlived the run: "
                    + ", ".join(f"{r['pid']} {r['cmd'][:60]}" for r in leaked[:5]))
                if not dry:
                    kill_leaked(leaked)
                attention(tid, c["branch"], "LEAKED",
                          f"{len(leaked)} process(es) outlived the run and were killed (SIGTERM then SIGKILL): "
                          + ", ".join(f"{r['pid']} {r['cmd'][:70]}" for r in leaked[:5]))
        except Exception as e:
            log(f"leak check error for {tid}: {e}")
        if c.get("deflake"):
            reap_deflake(claims, tid, c)       # its own outcomes: not a board ticket, no result: block
            continue
        d = f"{WORKDIR}/{tid}"
        if c["kind"] == "review":
            res = result_of(f"{d}/review.json")
            text = str(res.get("result", ""))
            if "VERDICT: PASS" in text:
                c["state"] = "queued"
                record_done(c, "review-pass", res)
                enqueue(c["branch"], c.get("wt"))
            else:
                fail = next((l for l in text.splitlines() if l.startswith("VERDICT: FAIL")), "no verdict line")
                record_done(c, "review-fail", res)
                # A review FAIL names a concrete defect; the worker that wrote the code fixes it with
                # its context intact, same path as a gate failure, same attempt cap.
                if c.get("session_id") and c.get("fix_attempts", 0) < FIX_ATTEMPTS and os.path.isdir(c.get("wt", "")):
                    claims[tid] = launch_fix(dict(c, kind="work"), f"REVIEW_FAIL {fail[:300]} (full review: {d}/review.json)")
                else:
                    c["state"] = "review-failed"
                    attention(tid, c["branch"], "REVIEW_FAIL", f"{fail[:200]} (full text: {d}/review.json)")
            continue
        res = result_of(c.get("out") or f"{d}/out.json")
        text = str(res.get("result", ""))
        if res.get("session_id"):
            c["session_id"] = res["session_id"]      # what a gate-failure fix resumes
        hb, hb_err = load_handback(d, tid)
        outcome, why = handback_outcome(hb, text)
        red, own = hand_back_reds(hb)
        if hb and outcome == "done" and own:
            bad = own[0]
            outcome, why = "blocked", f"claimed done with a failing test: {bad.get('cmd')} exit {bad.get('exit')}"
        elif hb and outcome == "done" and red:
            attention(tid, c["branch"], "NOTE", "queued with a red the worker marks not its own (the gate arbitrates): "
                      + "; ".join(f"{t.get('cmd')} exit {t.get('exit')}" for t in red)[:300])
        # How the hand-back arrived is the contract's own reliability measure: `json` is the
        # contract, `line` the HANDBACK: fallback, `none` a worker that wrote neither (judged by
        # its commits alone). One line per reap in $HACKRIFF_OPS/handbacks.jsonl; the rate is
        # `jq -r .how handbacks.jsonl | sort | uniq -c`, and `briefed` says whether the brief asked.
        how = "json" if hb else ("line" if any(l.startswith("HANDBACK:") for l in text.splitlines()) else "none")
        if hb_err:
            log(f"HANDBACK {tid}: {hb_err} - {'falling back to the text line' if how == 'line' else 'NO_HANDBACK, judged by commits alone'} ({outcome})")
        with open(f"{S}/handbacks.jsonl", "a") as f:
            f.write(json.dumps({"ts": int(time.time()), "ticket": tid, "how": how, "outcome": outcome,
                                "briefed": os.path.exists(f"{d}/brief.md") and "handback.json" in open(f"{d}/brief.md").read()}) + "\n")
        ahead = int(sh(["git", "rev-list", "--count", f"main..{c['branch']}"]).strip() or 0)
        dirty = [l for l in sh(["git", "status", "--porcelain"], cwd=c["wt"]).splitlines() if not l.startswith("??")] if os.path.isdir(c["wt"]) else []
        if c.get("host"):
            dirty = c.get("remote_dirty", [])        # this Mac's copy was just reset to the pushed branch
        if hb and outcome in ("done", "cancel") and ahead > 0 and not dirty:
            write_result(c, hb)                    # the board line the worker used to write by hand
            ahead = int(sh(["git", "rev-list", "--count", f"main..{c['branch']}"]).strip() or 0)
        if not hb and not res:
            # No result JSON and no handback: the run did not finish, it was KILLED (a signal - 04:07
            # on 2026-09-24 a pkill took five workers; they were logged "NO_HANDBACK ... (done)", parked
            # as uncommitted/no-work and never run again). Keep the worktree and resume the session.
            record_done(c, "killed", res)
            if c.get("session_id") and c.get("kill_resumes", 0) < KILL_RESUMES and os.path.isdir(c.get("wt", "")):
                claims[tid] = launch_fix(dict(c, kind="work"), f"KILLED your run ended after {age_min:.0f} min with no result - "
                                         f"it was killed by a signal, not failed; {len(dirty)} modified files and {ahead} commits "
                                         f"are in {c['wt']}: check them and continue the ticket from there")
                killed.append(f"{tid} ({'fix held' if claims[tid].get('state') == 'fix-held' else 'resumed'})")
            else:
                c["state"] = "killed"
                attention(tid, c["branch"], "KILLED", f"killed after {age_min:.0f} min with no session to resume; "
                          f"worktree kept ({len(dirty)} modified files, {ahead} commits) - redispatch it")
                killed.append(f"{tid} (needs a redispatch)")
            continue
        if res.get("is_error"):
            c["state"] = "error"
            attention(tid, c["branch"], "ERROR", f"claude -p reported an error after {age_min:.0f} min; see {d}/run.log")
            record_done(c, "error", res)
        elif outcome == "blocked":
            c["state"] = "blocked"
            attention(tid, c["branch"], "BLOCKED", (why or "")[:220])
            record_done(c, "blocked", res)
        elif outcome == "cancel":
            if ahead > 0 and not dirty:
                # The worker recorded the cancellation on its branch: an Opus reviewer confirms the
                # evidence whatever the worker's model, then it lands through the gate like code.
                record_done(c, "cancel-to-review", res)
                claims[tid] = dict(launch_review(dict(c, cancel_reason=why)), state="running")
            else:
                c["state"] = "cancel-proposed"
                attention(tid, c["branch"], "CANCEL_PROPOSED", why or "no reason given")
                record_done(c, "cancel-proposed", res)
        elif dirty:
            record_done(c, "uncommitted", res)
            if c.get("session_id") and c.get("fix_attempts", 0) < FIX_ATTEMPTS:
                claims[tid] = launch_fix(dict(c, kind="work"), f"UNCOMMITTED {len(dirty)} modified files left uncommitted in {c['wt']} (ahead={ahead}): finish and COMMIT them if they are the ticket's work and tests pass, otherwise `git checkout -- .` and hand back BLOCKED with why")
            else:
                c["state"] = "uncommitted"
                attention(tid, c["branch"], "UNCOMMITTED", f"{len(dirty)} modified files left uncommitted in {c['wt']}; ahead={ahead}")
        elif ahead == 0:
            c["state"] = "no-work"
            attention(tid, c["branch"], "NO_WORK", f"worker exited after {age_min:.0f} min with no commits; see {d}/out.json")
            record_done(c, "no-work", res)
        elif c.get("review"):
            record_done(c, "done-to-review", res)
            claims[tid] = dict(launch_review(c), state="running")
        else:
            c["state"] = "queued"
            record_done(c, "done", res)
            enqueue(c["branch"], c.get("wt"))
    if killed:
        alert("amber", f"{len(killed)} worker(s) killed", ", ".join(killed) + " - worktrees kept", "wr:killed:" + ",".join(sorted(killed)))
    changed |= handle_gate_failures(claims, dry)
    return changed


def fix_reason(fail_line, branch):
    """(class, reason) for a fix run - hkpy.fixes; a GATE_FAIL names what the merge runner's triage
    found red. Never fails a launch: an unreadable reason is OTHER with the raw line."""
    try:
        if f"{REPO}/py" not in sys.path:
            sys.path.append(f"{REPO}/py")
        from hkpy import fixes
        text = ""
        if fixes.classify(fail_line) == "GATE_FAIL" and os.path.exists(MERGE_LOG):
            with open(MERGE_LOG, "rb") as f:
                f.seek(max(0, os.path.getsize(MERGE_LOG) - 4_000_000))
                text = f.read().decode("utf-8", "replace")
        return fixes.reason_for(fail_line, text, branch)
    except Exception:
        return "OTHER", " ".join(fail_line.split())[:200]


def _gone(pid, wait_s=30):
    """The stopped run's whole process group has exited (SIGKILL after wait_s): a resume must not share the
    session with it. The leader is this runner's own unreaped Popen child - os.kill(pid, 0) succeeds on a
    zombie - so reap it first, and ask the GROUP, not the cpulimit wrapper (review, 2026-09-24)."""
    for i in range(wait_s + 3):
        try:
            os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            pass
        try:
            os.killpg(pid, 0)
        except ProcessLookupError:
            return True
        except PermissionError:
            pass
        if i == wait_s:
            try:
                os.killpg(pid, signal.SIGKILL)
            except OSError:
                pass
        time.sleep(1)
    return False


def launch_fix(c, fail_line):
    tid, branch, wt = c["ticket"], c["branch"], c["wt"]
    d = f"{WORKDIR}/{tid}"
    n = c.get("fix_attempts", 0) + 1
    cls, why = fix_reason(fail_line, branch)
    c = dict(c, fix_reason_class=cls, fix_reason=why)
    # A fix run is a dispatch. It used to bypass every hold: at 15:19 on 2026-09-22, with
    # dispatch-paused in force and the box meant to be empty for the gate, a GATE_FAIL on
    # task-t700 resumed a worker to "fix" a defect that was main's, not the branch's.
    if os.path.exists(f"{S}/dispatch-paused") or gate_holds_dispatch():
        attention(tid, branch, "FIX_HELD", f"fix attempt {n} NOT launched: dispatch is paused/gate pending ({fail_line[:160]})")
        return dict(c, state="fix-held", fail_line=fail_line[:300])
    if fail_line.startswith("KILLED"):
        k = c.get("kill_resumes", 0) + 1
        prompt = f"""Your run on {tid} was KILLED from outside (a signal - not a failure of yours, not a gate result);
this resumes the same session. {fail_line[len("KILLED "):]}.
In {wt}: `git status` and `git log --oneline main..{branch}` show what you had done. Keep what is right, then carry on with the
ticket exactly as your original brief says: targeted tests, commit on {branch}, write {d}/handback.json, and end your final
message with HANDBACK: DONE or HANDBACK: BLOCKED <why>. Same rules as before: never touch the main checkout, never the
full gate, never edit docs/tasks.yaml by hand.
"""
        r = _run_fix(dict(c, fix_reason_class="KILLED"), c.get("fix_attempts", 0), prompt, out_name=f"resume{k}.json", fail_line=fail_line)
        return r if r.get("state") == "fix-held" else dict(r, kill_resumes=k)
    if fail_line.startswith("TIMEOUT"):
        t = c.get("timeout_resumes", 0) + 1
        prompt = f"""Your run on {tid} reached its {MAX_MINUTES}-minute limit and was stopped; this resumes the same session ONCE,
with the same limit, to WRAP UP - not to continue open-ended.
In {wt}: `git status` and `git log --oneline main..{branch}` show what you have. Start no new scope. Get what is done into a
committed, tested state: targeted tests only, commit on {branch}, write {d}/handback.json. If the ticket's acceptance is met,
hand back DONE. If it is not, hand back BLOCKED and say precisely what remains (files, tests, the next step) in
blocked.needs, so the coordinator can split or re-brief it - a clear remainder is a good outcome here, a half-commit is not.
End your final message with HANDBACK: DONE or HANDBACK: BLOCKED <why>. Same rules as before: never touch the main
checkout, never the full gate, never edit docs/tasks.yaml by hand.
"""
        r = _run_fix(dict(c, fix_reason_class="TIMEOUT"), c.get("fix_attempts", 0), prompt, out_name=f"wrapup{t}.json", fail_line=fail_line)
        return r if r.get("state") == "fix-held" else dict(r, timeout_resumes=t)
    target = merge_target()
    target_note = "" if target == "main" else " - the last gated main; main itself holds a batch still gating"
    if is_conflict(fail_line):
        prompt = f"""Your branch {branch} was SKIPPED by the merge runner: it no longer merges cleanly into main
(fix attempt {n} of {FIX_ATTEMPTS}). The runner's line:
{fail_line}
In your worktree {wt}: `git merge {target}`{target_note}, resolve every conflict keeping BOTH sides' intent (main's change
is already gated and landed - never revert it; re-apply your change on top of it), then re-run the
targeted tests for every crate or suite the resolved files touch. docs/tasks.yaml: never hand-resolve it;
take main's copy (`git checkout {target} -- docs/tasks.yaml`) and re-apply your ticket's own fields with
`just task set/result/note`.
Commit the merge on {branch}. If the conflict shows main already did your ticket's work, or the two changes
cannot both hold, say so precisely and hand back BLOCKED.
Same rules as before: never touch the main checkout, never the full gate, never edit docs/tasks.yaml by hand.
When finished, REWRITE {d}/handback.json (same shape as before: outcome done|blocked, summary, commits, tests) and end
your final message with one line HANDBACK: DONE or HANDBACK: BLOCKED <why>.
"""
        return _run_fix(c, n, prompt, fail_line=fail_line)
    kind = "its REVIEW" if fail_line.startswith("REVIEW_FAIL") else "its merge gate on main"
    prompt = f"""Your branch {branch} FAILED {kind} (fix attempt {n} of {FIX_ATTEMPTS}). The finding:
{fail_line}
If this is a review finding, fix exactly what it names (the reviewer's full text is in the file it cites), then
re-run the targeted tests and hand back; the branch is reviewed again before it is queued.
The full gate log is {MERGE_LOG}; find your run with `grep -n 'GATE FAILED {branch}\\|FAIL \\[\\|FAILED just\\|error\\[' {MERGE_LOG} | tail -40`.
TRIAGE FIRST, in your worktree {wt}: merge main in (`git merge {target}`{target_note}), then run the failing test ALONE
(`cargo nextest run -p <crate> -E 'test(/<name>/)'` or `just test-ui`). Fails alone = a real bug: fix it.
Passes alone but failed in the gate = load-sensitive: make it deterministic (never a retry, never a skip).
If the failure is in code you did not touch and is a known bug on main, say so precisely and hand back BLOCKED.
Then: targeted tests, `just precheck <crates>`, commit on {branch}, `just task note {tid} --text "<what the gate found and what you changed>"`.
Same rules as before: never touch the main checkout, never the full gate, never edit docs/tasks.yaml by hand.
When finished, REWRITE {d}/handback.json (same shape as before: outcome done|blocked, summary, commits, tests) and end
your final message with one line HANDBACK: DONE or HANDBACK: BLOCKED <why>.
"""
    return _run_fix(c, n, prompt, fail_line=fail_line)


def _run_fix(c, n, prompt, out_name=None, fail_line=""):
    tid, wt = c["ticket"], c["wt"]
    d = f"{WORKDIR}/{tid}"
    cmd = ["claude", "-p", "--resume", c["session_id"], "--model", c.get("model", "sonnet"), "--dangerously-skip-permissions",
           "--output-format", "json", "--max-budget-usd", BUDGET_USD]
    out_path = f"{d}/{out_name or f'fix{n}.json'}"
    if c.get("host"):      # the session lives on that host: a resume runs there, once the previous run is gone
        host, pname = c["host"], f"prompt-{os.path.basename(out_path)}.md"
        try:
            if not remote_stop(c):
                raise RuntimeError("the previous remote run could not be confirmed stopped")
            remote_prepare(host, wt, c["branch"], d, resume=True)
            remote_put(host, f"{d}/{pname}", prompt)
            if os.path.exists(f"{d}/review.json"):             # the Mac-side files a fix prompt points at
                remote_put(host, f"{d}/review.json", open(f"{d}/review.json").read())
            try:
                remote_put(host, MERGE_LOG, "".join(open(MERGE_LOG).readlines()[-20000:]))
            except OSError:
                pass
            script = "exec " + " ".join(f"'{a}'" for a in cmd) + f" < '{d}/{pname}'"
            p = remote_popen(host, wt, c["branch"], script, dict(CARGO_ENV, HK_WORKER="1", HACKRIFF_OPS=S, **e2e_env(wt)),
                             d, os.path.basename(out_path), open(out_path, "w"), open(f"{d}/run.log", "a"))
        except Exception as e:
            log(f"FIX {tid} on {host} NOT launched: {e}")
            if not c.get("held_warned"):          # once per hold, not every tick while the host is away
                attention(tid, c["branch"], "FIX_HELD", f"fix attempt {n} on {host} not launched: {str(e)[:200]}")
            return dict(c, state="fix-held", fail_line=fail_line or c.get("fail_line") or "", held_warned=True)
        log(f"FIX {tid} attempt {n} [{c.get('fix_reason_class', 'OTHER')}] on {host}: resumed session {c['session_id'][:8]} pid={p.pid}")
        return dict(c, pid=p.pid, started=time.time(), kind="fix", state="running", out=out_path, fix_attempts=n,
                    fail_line=None, held_warned=False, detached=False)
    out = open(out_path, "w")
    err = open(f"{d}/run.log", "a")
    p = subprocess.Popen(bounded(cmd), cwd=wt, stdin=subprocess.PIPE, stdout=out, stderr=err,
                         env=dict(os.environ, **CARGO_ENV, HK_WORKER="1", HACKRIFF_OPS=S, **e2e_env(wt)), start_new_session=True, text=True)
    p.stdin.write(prompt)
    p.stdin.close()
    log(f"FIX {tid} attempt {n} [{c.get('fix_reason_class', 'OTHER')}] {c.get('fix_reason', '')[:120]}: "
        f"resumed session {c['session_id'][:8]} pid={p.pid} (bounded)")
    return dict(c, pid=p.pid, started=time.time(), kind="fix", state="running", out=out_path, fix_attempts=n)


def branches_waiting():
    """Branches in merge-queue.txt or in the running batch (the bulk marker's branches=)."""
    waiting = set()
    for f in (MERGE_QUEUE, BULKMARK):
        try:
            waiting |= set(open(f).read().replace("branches=", " ").split())
        except OSError:
            pass
    return waiting


def release_stale_claims(claims, tasks_by_id):
    changed = False
    # A `queued` claim whose branch is already on main is finished: the merge runner landed it
    # (or a hand-merge did) and nothing flipped the claim. 24 of 39 "queued" claims were such
    # on 2026-09-23 01:40, inflating the dashboard's IN QUEUE count and every throughput read.
    # Not while main is provisional: a batch commits each merge before gating (and a single merge is
    # staged), so every branch in it reads "on main" and was closed - T-866 at 03:33:46 on 2026-09-24,
    # 16 s before that batch failed, which left its red with no claim to resume. The reaper has the
    # same guard for the same reason.
    provisional = os.path.exists(BULKMARK) or os.path.exists(f"{REPO}/.git/MERGE_HEAD")
    for tid, c in list(claims.items()):
        b = c.get("branch")
        if c.get("state") == "queued" and b and not provisional:
            try:
                if sh(["git", "rev-parse", "-q", "--verify", b]).strip() and \
                   int(sh(["git", "rev-list", "--count", f"main..{b}"]).strip() or 0) == 0:
                    log(f"CLAIM {tid}: {b} is on main - claim closed")
                    c["state"] = "merged"; c["ended"] = time.time(); changed = True
            except Exception:
                pass
    # ...and one whose TICKET is done on the board while its branch is in no queue: it landed as a
    # rebuilt copy (task-t538 as task-t538-rl), so its own branch is never on main. 12 of 13 `queued`
    # claims were such on 2026-09-24 02:50, 26-42 h after landing - each one a deflake wait on a spec
    # its branch touched (deflake_deferred) and a phantom in every "in queue" count.
    # A staged single-branch merge is in neither list: wait for it to end (the next tick decides).
    waiting = None if os.path.exists(f"{REPO}/.git/MERGE_HEAD") else branches_waiting()
    for tid, c in list(claims.items()):
        if (waiting is not None and c.get("state") == "queued" and c.get("branch") and c["branch"] not in waiting
                and tasks_by_id.get(tid, {}).get("status") in ("done", "cancelled")):
            log(f"CLAIM {tid}: the board says {tasks_by_id[tid]['status']} and {c['branch']} is in no queue "
                f"(landed as a rebuilt branch) - claim closed")
            c["state"] = "merged"; c["ended"] = time.time(); changed = True
    for tid, c in list(claims.items()):
        if c.get("state") in ("no-work", "error", "timeout", "killed") and time.time() - c.get("started", 0) > RELEASE_AFTER_H * 3600:
            if tasks_by_id.get(tid, {}).get("status") == "todo":
                log(f"RELEASE {tid}: claim ended {c['state']} {RELEASE_AFTER_H:.0f}h+ ago and the ticket is still todo - eligible again")
                del claims[tid]; changed = True
    return changed


def merge_target():
    """What a fix merges and is tested against: main - except while a batch gates, when main holds
    that ungated batch and may be rewound; then the bulk marker's base=, the last gated main."""
    try:
        for ln in open(BULKMARK):
            if ln.startswith("base="):
                return ln.split("=", 1)[1].strip() or "main"
    except OSError:
        pass
    return "main"


def board_statuses():
    try:
        return {t["id"]: t.get("status") for t in board()}
    except Exception:
        return {}


def commits_ahead(branch, target):
    try:
        return int(sh(["git", "rev-list", "--count", f"{target}..{branch}"]).strip() or 0)
    except Exception:
        return -1


def _line_ts(line):
    try:
        return time.mktime(time.strptime(f"{time.localtime().tm_year} {line[:11]}", "%Y %m-%d %H:%M"))
    except ValueError:
        return None


def conflict_skip(c, branch, line, statuses):
    """Why a CONFLICT line needs no fix run, or None. The first replay against the real files found
    13 unseen lines of which 11 were tickets already done on main (re-landed under -rl branches)."""
    if statuses.get(c["ticket"]) in ("done", "cancelled", "cancel-proposed"):
        return f"ticket is {statuses.get(c['ticket'])} on main"
    t = _line_ts(line)
    if t is not None and t < c.get("started", 0) - 60:
        return "the line predates the claim's latest run"
    if branch in queued_branches():
        return "queued again"
    if branch in merging_branches():
        return "being merged now"
    target = merge_target()
    if commits_ahead(branch, target) == 0:
        return "nothing ahead of main"
    if merges_cleanly(branch, target):
        return "clean"
    return None


def merging_branches():
    """Branches the merge runner is merging right now: a batch member whose tip is IN main's HEAD
    (the batch commits each merge before gating), or the single merge staged in main (MERGE_HEAD's
    tip). Such a branch is not in merge-queue.txt, so without this the conflict rule re-queued
    task-t613 at 17:06 while its own gate was running. NOT every name in the marker's `branches=`:
    that lists every branch the batch ATTEMPTED, including the ones it skipped for a conflict - read
    as "being merged", T-848 and T-849 (skipped 19:16) were never given their fix run."""
    out = set()
    try:
        for ln in open(BULKMARK):
            if ln.startswith("branches="):
                for b in ln.split("=", 1)[1].split():
                    if subprocess.run(["git", "merge-base", "--is-ancestor", b, "HEAD"], cwd=REPO,
                                      capture_output=True, timeout=30).returncode == 0:
                        out.add(b)
    except (OSError, subprocess.SubprocessError):
        pass
    try:
        head = open(f"{REPO}/.git/MERGE_HEAD").read().split()[0]
        out.update(b for b in sh(["git", "branch", "--format=%(refname:short)", "--points-at", head]).split())
    except (OSError, IndexError):
        pass
    return out


def queued_branches():
    try:
        return {l.strip() for l in open(f"{S}/merge-queue.txt") if l.strip() and not l.lstrip().startswith("#")}
    except OSError:
        return set()


def merges_cleanly(branch, target="main"):
    """`git merge-tree --write-tree` (git >= 2.38) merges in memory: exit 0 clean, 1 conflicted.
    Anything else (an old git, a missing branch) answers False - the fix run then decides."""
    try:
        return subprocess.run(["git", "-C", REPO, "merge-tree", "--write-tree", target, branch],
                              capture_output=True, timeout=60).returncode == 0
    except Exception:
        return False


def is_conflict(line):
    """A merge-runner CONFLICT line (`CONFLICT` or `CONFLICT(skipped from bulk)`) - a branch main moved
    past. It used to wait for the coordinator: median 3.5 h from skip to landing over 16 tickets on
    2026-09-22/23, and 20 more conflicted branches never landed. The worker that wrote the branch is
    the cheapest one to re-apply it on main, exactly like a gate failure."""
    parts = line.split()
    return len(parts) > 4 and parts[4].startswith("CONFLICT")


def handle_gate_failures(claims, dry):
    """Merge-runner GATE_FAIL and CONFLICT lines for branches this runner queued -> resume the worker to fix.
    Also: claims left in review-failed (from before the review-fix path existed) get the same path."""
    for tid, c in list(claims.items()):
        if c.get("deflake"):
            continue                           # a deflake review FAIL is escalated at reap, never resumed
        if c.get("state") == "review-failed" and c.get("session_id") and c.get("fix_attempts", 0) < FIX_ATTEMPTS and os.path.isdir(c.get("wt", "")):
            try:
                text = str(result_of(f"{WORKDIR}/{tid}/review.json").get("result", ""))
                fail = next((l for l in text.splitlines() if l.startswith("VERDICT: FAIL")), "VERDICT: FAIL (see review.json)")
            except Exception:
                fail = "VERDICT: FAIL (see review.json)"
            if not dry:
                claims[tid] = launch_fix(dict(c, kind="work"), f"REVIEW_FAIL {fail[:300]} (full review: {WORKDIR}/{tid}/review.json)")
    try:
        lines = [l.rstrip("\n") for l in open(MERGE_NEEDS) if "GATE_FAIL" in l or is_conflict(l)]
    except FileNotFoundError:
        return False
    by_branch = {c["branch"]: tid for tid, c in claims.items() if c.get("branch")}
    changed = False
    conflict_runs, statuses = 0, None
    for line in lines:
        parts = line.split()
        branch = parts[2] if len(parts) > 2 else ""
        tid = by_branch.get(branch)
        if not tid:
            continue
        c = claims[tid]
        if line in c.get("gate_fails_seen", []) or c.get("state") != "queued":
            continue
        if c.get("deflake") and not is_conflict(line):
            # A deflake branch has no ticket to note and no worker brief a fix run could resume
            # against; its red goes to a person, and the ledger's next request re-dispatches it.
            c.setdefault("gate_fails_seen", []).append(line)
            c["state"], c["ended"] = "gate-failed", time.time()
            changed = True
            attention(tid, branch, "DEFLAKE_GATE_FAIL", f"{line[:200]} - not resumed automatically")
            continue
        if is_conflict(line) and not dry:
            if statuses is None:
                statuses = board_statuses()
            if not statuses:
                continue                      # board unreadable: cannot tell a landed ticket, so wait
            why = conflict_skip(c, branch, line, statuses)
            if why == "being merged now":
                continue                      # decided when that merge ends: lands, or conflicts again
            if why:
                c.setdefault("gate_fails_seen", []).append(line)
                changed = True
                if why == "clean":
                    enqueue(branch, c.get("wt"))
                    why = "merges cleanly now - re-queued"
                log(f"CONFLICT {tid}: no fix run - {why} ({line[:80]})")
                continue
            if c.get("deflake"):
                # Where a ticket would get a fix run: escalate instead (no ticket brief to resume), no slot spent.
                c.setdefault("gate_fails_seen", []).append(line)
                c["state"], c["ended"] = "conflict", time.time()
                changed = True
                attention(tid, branch, "DEFLAKE_CONFLICT", f"{line[:200]} - does not merge cleanly; not resumed automatically")
                continue
            # A conflict run is a worker: it waits for a slot under the dispatch cap, one per tick,
            # and never while a hold is in force (a held one would all relaunch at once). The line
            # stays unseen until then, so a backlog cannot burst.
            if (conflict_runs >= 1 or busy_workers(claims) >= dispatch_cap()
                    or os.path.exists(f"{S}/dispatch-paused") or gate_holds_dispatch()):
                continue
            c.setdefault("gate_fails_seen", []).append(line)
            changed = True
            if not c.get("session_id") or not os.path.isdir(c.get("wt", "")):
                attention(tid, branch, "CONFLICT_NO_SESSION", "no worker session or worktree to resume; needs a person")
            elif c.get("fix_attempts", 0) >= FIX_ATTEMPTS:
                attention(tid, branch, "CONFLICT_ESCALATE", f"{FIX_ATTEMPTS} fix attempts spent; needs a person")
            else:
                conflict_runs += 1
                claims[tid] = launch_fix(c, line)
            continue
        c.setdefault("gate_fails_seen", []).append(line)
        changed = True
        if dry:
            log(f"DRY-RUN would resume {tid} to fix: {line}")
            continue
        if not c.get("session_id") or not os.path.isdir(c.get("wt", "")):
            c["state"] = "gate-failed"
            attention(tid, branch, "GATE_FAIL_NO_SESSION", "no worker session or worktree to resume; needs a person")
        elif c.get("fix_attempts", 0) >= FIX_ATTEMPTS:
            c["state"] = "gate-failed"
            attention(tid, branch, "GATE_FAIL_ESCALATE", f"{FIX_ATTEMPTS} fix attempts spent; needs a person")
        else:
            claims[tid] = launch_fix(c, line)
    return changed


# ---------- board sync ----------
def has_work(tid):
    """A branch with commits ahead of main, or a dirty worktree: someone's work (same test as candidates)."""
    try:
        if sh(["git", "rev-parse", "--verify", "-q", branch_of(tid)]).strip() and \
           int(sh(["git", "rev-list", "--count", f"main..{branch_of(tid)}"]).strip() or 0) > 0:
            return True
        wt = worktree_of(tid)
        return os.path.isdir(wt) and any(not l.startswith("??") for l in sh(["git", "status", "--porcelain"], cwd=wt).splitlines())
    except Exception:
        return True                       # cannot tell: treat as work, never revert


def dead_dispatches(claims, tasks, now, has_work=has_work, busy=None, prior=None):
    """([revert], [skipped]) - tickets THIS runner flipped to in-progress whose run ended with nothing
    to show: no-work, error or timeout, RELEASE_AFTER_H ago, no commits, no edits. They go back to
    todo; release_stale_claims then frees the claim and dispatch sees them again.
    Before this rule the flip was one-way: release_stale_claims only frees a claim whose ticket is
    `todo`, so a dispatch killed two minutes in (T-801, 2026-09-22 14:38, the stop-kill pattern)
    held its ticket in-progress for 26 h and with it the 19 MMAP tickets that depend on it.
    Guards (review): `busy` = {ticket: latest agent-registry ts} - an agent spawned on the ticket
    after the claim started is someone's work; a board `branch:` other than the runner's own means
    a person took it; and one revert per ticket - `prior` counts its earlier dead outcomes in
    work-done.jsonl, and a second dead run is `skipped` for a person rather than looped."""
    busy, prior = busy or {}, prior or {}
    revert, skipped = [], []
    for tid, c in claims.items():
        t = tasks.get(tid)
        if not (t and t.get("status") == "in-progress" and c.get("state") in ("no-work", "error", "timeout", "killed")
                and now - c.get("started", 0) > RELEASE_AFTER_H * 3600):
            continue
        if busy.get(tid, 0) > c.get("started", 0) or t.get("branch") not in (None, branch_of(tid)) or has_work(tid):
            continue
        (revert if prior.get(tid, 0) <= 1 else skipped).append(tid)
    return revert, skipped


def dead_outcomes():
    """{ticket: n} of work runs that ended no-work/error/timeout, from work-done.jsonl."""
    n = {}
    try:
        for line in open(DONE):
            try:
                o = json.loads(line)
            except ValueError:
                continue
            if o.get("kind") == "work" and o.get("outcome") in ("no-work", "error", "timeout"):
                n[o.get("ticket")] = n.get(o.get("ticket"), 0) + 1
    except OSError:
        pass
    return n


def sync_board(claims, dry):
    """Flip statuses on main in one small commit - under `$S/board-sync.lock`, because two
    processes now do this: the daemon every tick, and the merge runner's `--sync-board` right
    after a landing (review, 2026-09-23: both see main safe at the same instant, and an unlocked
    read-modify-write would commit a board that undoes the other's flips)."""
    import fcntl
    with open(f"{S}/board-sync.lock", "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        return _sync_board(claims, dry)


def _sync_board(claims, dry):
    """Flip statuses on main in ONE small commit, only when main is safe. Never edits anything else."""
    if not main_safe_to_commit():
        return
    tasks = {t["id"]: t for t in board()}
    landed = landed_tickets()
    flips = []
    for tid, c in claims.items():
        t = tasks.get(tid)
        if not t:
            continue
        if c.get("state") == "running" and c.get("kind") == "work" and t.get("status") == "todo":
            flips.append((tid, "in-progress", None))
    # Agents the coordinator or supervisor spawned with the Agent tool are not claims, but the
    # PreToolUse(Agent) hook registered them (agent-registry.jsonl, user 2026-09-23): a ticket
    # with a registered agent spawned in the last 30 min and still `todo` is in progress too.
    agents_on = {}          # ticket -> latest spawn of an agent that does ticket work, within a worker's lifetime
    try:
        cut = time.time() - 1800
        life = time.time() - MAX_MINUTES * 60
        with open(f"{S}/agent-registry.jsonl") as f:
            lines = f.readlines()
        for line in lines[-200:]:
            o = json.loads(line)
            t = tasks.get(o.get("ticket") or "")
            if t and o.get("ts", 0) > cut and t.get("status") == "todo" and (o["ticket"], "in-progress", None) not in flips:
                flips.append((o["ticket"], "in-progress", None))
        for line in lines[-2000:]:
            o = json.loads(line)
            # a reviewer or an Explore that merely NAMES a ticket is not working it (this rule's own
            # review registered against T-801)
            if o.get("ticket") and o.get("type") in ("worker", "deflaker", "general-purpose", "claude") and o.get("ts", 0) > life:
                agents_on[o["ticket"]] = max(agents_on.get(o["ticket"], 0), o["ts"])
    except Exception:
        pass
    revert, skipped = dead_dispatches(claims, tasks, time.time(), has_work, agents_on, dead_outcomes())
    flips += [(tid, "todo", None) for tid in revert]
    for tid in skipped:
        try:
            already = f"  {tid}  REVERT_SKIPPED" in open(NEEDS).read()
        except OSError:
            already = False
        if not already:
            attention(tid, branch_of(tid), "REVERT_SKIPPED", "a second dispatch ended with nothing; left in-progress for a person (re-scope, block or cancel)")
    for tid, sha in landed.items():
        t = tasks.get(tid)
        if t and t.get("status") in ("todo", "in-progress"):
            flips.append((tid, "done", sha[:8] if sha else None))
    if not flips:
        return
    if dry:
        log("DRY-RUN would flip: " + ", ".join(f"{a}->{b}" for a, b, _ in flips))
        return
    path = f"{REPO}/docs/tasks.yaml"
    text = open(path).read()
    for tid, status, sha in flips:
        m = re.search(rf"^  - id: {re.escape(tid)}\n(.*?)(?=^  - id: |\Z)", text, re.M | re.S)
        if not m:
            continue
        block = m.group(0)
        new = re.sub(r"^    status: \S+$", f"    status: {status}", block, count=1, flags=re.M)
        if status == "done" and sha and not re.search(r"^    commit:", new, re.M):
            new = new.replace(f"    status: {status}\n", f"    status: {status}\n    commit: \"{sha}\"\n", 1)
        if status == "in-progress" and not re.search(r"^    branch:", new, re.M):
            new = new.replace("    status: in-progress\n", f"    status: in-progress\n    branch: {branch_of(tid)}\n", 1)
        text = text.replace(block, new, 1)
    open(path, "w").write(text)
    try:
        yaml.safe_load(text)
    except yaml.YAMLError as e:
        sh(["git", "checkout", "HEAD", "--", "docs/tasks.yaml"])
        attention("board", "main", "SYNC_YAML_BROKEN", str(e)[:200])
        return
    if gate_running():  # re-check right before committing: a bulk may have started during the edit
        sh(["git", "checkout", "HEAD", "--", "docs/tasks.yaml"])
        return
    sh(["git", "add", "docs/tasks.yaml"])
    msg = "Board: work-runner status sync - " + ", ".join(f"{a} {b}" for a, b, _ in flips)
    r = subprocess.run(["git", "commit", "-q", "-m", msg], cwd=REPO, capture_output=True, text=True)
    if r.returncode != 0:
        sh(["git", "checkout", "HEAD", "--", "docs/tasks.yaml"])
        attention("board", "main", "SYNC_COMMIT_FAILED", r.stderr.strip()[:200])
    else:
        log("BOARD " + msg)


# ---------- dispatch ----------
def candidates(tasks, claims):
    by_id = {t["id"]: t for t in tasks}
    running = [c for c in claims.values() if c.get("state") == "running"]
    per_group = {}
    for c in running:
        if c.get("group"):
            per_group[c["group"]] = per_group.get(c["group"], 0) + 1
    busy_groups = {g for g, n in per_group.items() if n >= GROUP_CAP}
    out = []
    for t in tasks:
        tid = t["id"]
        if t.get("status") != "todo" or tid in claims:
            continue
        if t.get("needs") in ("user", "hardware") or t.get("blocked_on") or t.get("dispatch") == "manual":
            continue
        if any(by_id.get(d, {}).get("status") not in ("done", "cancelled") for d in deps_of(t) if d in by_id):
            continue
        if t.get("parallel_group") in busy_groups:
            continue
        if sh(["git", "rev-parse", "--verify", "-q", branch_of(tid)]).strip():
            # A branch with commits, or a dirty worktree, is someone's work: leave it. A branch with
            # NO commits and a clean (or absent) worktree is a dispatch that never ran - this runner
            # killed mid-tick on 2026-09-22 left exactly that - and launch() reuses it.
            ahead = int(sh(["git", "rev-list", "--count", f"main..{branch_of(tid)}"]).strip() or 0)
            wt = worktree_of(tid)
            dirty = os.path.isdir(wt) and any(not l.startswith("??") for l in sh(["git", "status", "--porcelain"], cwd=wt).splitlines())
            if ahead or dirty:
                continue
        out.append(t)
    out.sort(key=lambda t: (not is_user(t), PRI.get(t.get("priority", "normal"), 2), ticket_num(t["id"])))
    return out


_GATE_SEEN = [0.0]
RESERVE_GRACE_S = 90   # > one tick (30 s) + the runner's gap between two gates (3-8 s, then its 8 s sleep)


def dispatch_cap():
    # The gate keeps its GATE_RESERVE cores while it runs - and for RESERVE_GRACE_S after it was last
    # seen: an isolation's single gates leave 3-8 s gaps with no marker, and each gap a tick landed in
    # filled the box to CAP (2026-09-24 14:30: 7 workers beside a gate, load 58 vs plan 32). Not keyed
    # on the queue: held/parked branches sit there with no gate coming (review).
    if GATE_ALONE:
        return CAP
    if gate_running():
        _GATE_SEEN[0] = time.time()
    return min(CAP, RESERVE_CAP) if time.time() - _GATE_SEEN[0] < RESERVE_GRACE_S else CAP


def busy_workers(claims):
    """Running work, fix AND deflake runs: each is a worker on the box (dispatch counted only `work`)."""
    return sum(1 for c in claims.values() if c.get("state") == "running" and c.get("kind") in ("work", "fix", "deflake"))


def dispatch(claims, dry):
    running = [c for c in claims.values() if c.get("state") == "running" and c.get("kind") == "work"]
    cap = dispatch_cap()
    free = cap - busy_workers(claims)          # fix runs are workers on the box too
    if free <= 0:
        return False
    if disk_free_gb() < DISK_MIN_GB:
        log(f"HOLD: {disk_free_gb():.0f} GB free < {DISK_MIN_GB} GB floor")
        return False
    load1 = os.getloadavg()[0]
    if load1 > LOAD_MAX:
        log(f"HOLD: load {load1:.0f} > {LOAD_MAX:.0f} tripwire ({len(running)} running)")
        return False
    # THE GATE GETS THE BOX TO ITSELF (user, 2026-09-22). Two rules, one cycle:
    #   1. no dispatch while a gate runs (the merge runner only starts one once no worker is
    #      running - see merge-runner.sh workers_running) - so a gate never shares the box;
    #   2. no dispatch once QUEUE_PAUSE branches wait in merge-queue.txt - running workers
    #      finish and join the queue, the box empties, the gate takes the batch.
    # Nothing is suspended; a worker that has started always runs to its hand-back.
    # HARD PAUSE (user, 2026-09-22, "have we fully paused new development yet?"): a file, not a
    # condition. Every conditional hold above has a gap (the bulk marker cleared and four workers
    # started inside a minute, 14:36); this one has none. Create $HACKRIFF_OPS/dispatch-paused
    # to stop all dispatch; delete it to resume. Reaping, results and queueing carry on.
    if os.path.exists(f"{S}/dispatch-paused"):
        log(f"HOLD: dispatch-paused file present ({len(running)} running)")
        return False
    # The three holds below are the alone-mode cycle (WORK_GATE_ALONE=1, see gate_holds_dispatch).
    # In the default overlap mode a gate only lowers the cap to the reserve (above) and the queue
    # never pauses dispatch: the merge runner gates whatever is queued as soon as the previous
    # gate ends, so the batch is "what handed back during the last gate".
    if gate_holds_dispatch():
        log(f"HOLD: a gate is running ({len(running)} workers still finishing)")
        return False
    depth = queue_depth()
    if GATE_ALONE and depth >= QUEUE_PAUSE:
        log(f"HOLD: {depth} branches queued for merge >= {QUEUE_PAUSE}; letting {len(running)} workers drain so the gate can run alone")
        return False
    # The gate is IMMINENT when something is queued and no worker is running: the merge runner
    # starts it within seconds, and its bulk marker can land a tick after this check (21:02:21
    # marker vs 21:02:22 dispatch on 2026-09-22 - two workers built beside that gate). Do not
    # dispatch into that window; the gate takes the batch, then dispatch resumes.
    if GATE_ALONE and depth > 0 and not running:
        log(f"HOLD: {depth} branch(es) queued and no worker running - a gate is about to start")
        return False
    free = min(free, PER_TICK)
    try:
        tasks = board()
    except Exception as e:
        attention("board", "main", "BOARD_UNREADABLE", str(e)[:200])
        return False
    changed = False
    taken = {}   # launches per parallel_group this tick, on top of the running-claim count
    for t in candidates(tasks, claims):
        if free <= 0:
            break
        g = t.get("parallel_group")
        if g and taken.get(g, 0) + sum(1 for c in claims.values() if c.get("state") == "running" and c.get("group") == g) >= GROUP_CAP:
            continue
        c = launch(t, dry)
        if c or dry:
            if c:
                claims[t["id"]] = dict(c, state="running")
                changed = True
            if g:
                taken[g] = taken.get(g, 0) + 1
            free -= 1
    return changed


#: Untracked paths a worktree regenerates on its own - build output, installed hooks, envs. A
#: worktree with no commits ahead and no tracked edits whose ONLY untracked files are these holds
#: nothing of anyone's: t356 (claim `blocked`, 0 commits, only an untracked `.githooks/`) held
#: 31 GB and failed `worktree remove` on EVERY tick - 1,499 REAP failures by 2026-09-24 00:18,
#: beside 1,730 for gateaudit, a worktree whose directory was already gone.
REGENERABLE = (".githooks/", "target/", "node_modules/", "ui/node_modules/", "ui/dist/", "py/.venv/", ".venv/")
_REAP_SAID: set = set()


def only_regenerable(untracked):
    return bool(untracked) and all(any(u == r or u.startswith(r) for r in REGENERABLE) for u in untracked)


def reap_worktrees(claims, dry):
    """Disk is the binding resource (29 GB free on 2026-09-22, ~10 GB per built worktree), and the
    merge runner removes a worktree only when IT merges the branch. Orphans - killed sessions,
    no-work dispatches, merged-by-hand branches - stay forever. Remove a worktree when its branch
    is MERGED into main, or when it is clean with NO commits ahead and no live claim; never one
    that is dirty, has unmerged commits, belongs to a running claim, or is younger than
    REAP_AFTER_MIN. Branches are never deleted, only worktrees."""
    if os.path.exists(BULKMARK):
        return   # main is provisional during a bulk gate: "merged" cannot be trusted
    live = {c.get("wt") for c in claims.values() if c.get("state") == "running"}
    if not dry:
        sh(["git", "worktree", "prune"])   # entries whose directory is gone (gateaudit: 1,730 failures)
    out = sh(["git", "worktree", "list", "--porcelain"])
    paths = [l.split(" ", 1)[1] for l in out.splitlines() if l.startswith("worktree ") and "/.claude/worktrees/" in l]
    for wt in paths:
        if wt in live or not os.path.isdir(wt):
            continue
        if time.time() - os.path.getmtime(wt) < REAP_AFTER_MIN * 60:
            continue
        branch = sh(["git", "branch", "--show-current"], cwd=wt).strip()
        if not branch:
            continue
        status = sh(["git", "status", "--porcelain"], cwd=wt).splitlines()
        dirty = any(not l.startswith("??") for l in status)
        if dirty:
            continue
        untracked = [l[3:] for l in status if l.startswith("??")]
        ahead = int(sh(["git", "rev-list", "--count", f"main..{branch}"]).strip() or 0)
        if ahead:
            continue   # unmerged commits, whatever main says: 2026-09-22 a provisional bulk merge on main
                       # read as "merged" and this reaped a worktree holding 4 newer commits
        merged = bool(sh(["git", "log", "main", "--merges", "--format=%H", "--fixed-strings", "--grep", branch, "-n", "1"]).strip())
        if dry:
            log(f"DRY-RUN would reap worktree {wt} ({branch}: {'merged' if merged else 'no commits'})")
            continue
        # Untracked files block `worktree remove`. On a MERGED branch or a claim that ended without
        # commits (no-work / error / timeout) they are abandoned scratch: force. Otherwise refuse, and say so.
        claim = next((c for c in claims.values() if c.get("wt") == wt), {})
        force = merged or claim.get("state") in ("no-work", "error", "timeout") or only_regenerable(untracked)
        if untracked and not force:
            # Someone's uncommitted new files: never forced. Said ONCE (not on every tick).
            key = (wt, tuple(sorted(untracked)))
            if key not in _REAP_SAID:
                _REAP_SAID.add(key)
                log(f"REAP {wt} ({branch}: no commits) kept - untracked files that are not build output: {' '.join(untracked)[:160]}")
            continue
        r = subprocess.run(["git", "worktree", "remove"] + (["--force"] if force else []) + [wt], cwd=REPO, capture_output=True, text=True)
        log(f"REAP {wt} ({branch}: {'merged' if merged else 'no commits'}{', forced' if force else ''}) {'ok' if r.returncode == 0 else r.stderr.strip()[:120]}")


E2E_DATA_IDLE_MIN = 60   # a leaked browser-e2e backend data dir untouched this long is removed


def reclaim_e2e_data(dry):
    """ui/e2e/backend.mjs gives every spec's `hk serve` a mkdtemp data dir ($TMPDIR/hk-e2e-data-*,
    4-5 GB each: the IQ ring and pyramid) and removes it in stop() - which a spec killed by a gate
    timeout, an orphan sweep or a signal never runs. 2026-09-24 10:30: 49 such dirs, 89.6 GB, back to
    09-22, and free disk falling ~10 GB per 20 min; 88 GB came back by hand. A live backend names its
    dir in argv (--data-dir), so: no process names it and nothing in it written for E2E_DATA_IDLE_MIN."""
    tmp = tempfile.gettempdir()
    dirs = [os.path.join(tmp, n) for n in os.listdir(tmp) if n.startswith("hk-e2e-data-")]
    if not dirs:
        return
    cut = time.time() - E2E_DATA_IDLE_MIN * 60
    procs = sh(["ps", "-axo", "command"])
    if not procs.strip():
        return   # no process table: no evidence the dirs are unused
    for d in dirs:
        if d in procs or os.path.islink(d) or not os.path.isdir(d):
            continue
        try:
            newest = max([os.path.getmtime(d)] + [e.stat().st_mtime for e in os.scandir(d)])
        except OSError:
            continue
        if newest > cut:
            continue
        if dry:
            log(f"DRY-RUN would remove leaked e2e data dir {d}")
            continue
        shutil.rmtree(d, ignore_errors=True)
        log(f"RECLAIM {d} (leaked e2e backend data, idle {(time.time() - newest) / 60:.0f} min, no process names it)")


def _target_written(t):
    """Newest write under target/. ctime too: `cp -c -R -p` (the worktree clone recipe) keeps main's old
    mtimes, so a target cloned a minute ago would read as idle; the clone cannot keep the old ctime."""
    return max(max(st.st_mtime, st.st_ctime) for st in
               (os.stat(p) for p in (t, f"{t}/debug", f"{t}/debug/deps", f"{t}/debug/.fingerprint") if os.path.exists(p)))


_HASHED = re.compile(r"^(.+)-([0-9a-f]{16})$")


def _units(profile_dir):
    """hash -> the build unit it belongs to, from cargo's own `.fingerprint/<pkg>-<hash>/` directory: the
    package plus the target kind files it holds (`bin-hk`, `test-bin-hk`, `test-integration-test-api_contract`).
    A file stem is NOT a unit: `hk` is both hk-cli's bin and its unit-test harness, and four integration-test
    names exist in two crates each (review, 2026-09-24 - the stem-keyed first pass deleted current twins)."""
    # ...and the configuration: cargo gives the same unit a new hash per profile, and two are live at once -
    # workers build line-tables-only, gates the default dev profile (re-review, 2026-09-24: 1211 of 2344
    # "superseded" executables differed from the newest only in profile, and both were rebuilt daily).
    out, fp = {}, os.path.join(profile_dir, ".fingerprint")
    for d in os.listdir(fp) if os.path.isdir(fp) else []:
        m = _HASHED.match(d)
        if not m:
            continue
        try:
            files = os.listdir(os.path.join(fp, d))
            kinds = sorted({re.sub(r"^(dep|output)-", "", f) for f in files
                            if f != "invoked.timestamp" and not f.endswith(".json")})
            with open(os.path.join(fp, d, next(f for f in files if f.endswith(".json")))) as fh:
                j = json.load(fh)
        except (OSError, StopIteration, ValueError):
            continue
        out[m.group(2)] = (m.group(1), tuple(kinds), j.get("profile"), j.get("features"), str(j.get("rustflags")))
    return out


def superseded_executables(profile_dir):
    """Every executable in `<profile>/deps` whose build unit has a NEWER executable there: the same unit at an
    older hash, which cargo never runs again (and rebuilds if it is ever wanted). An executable whose unit
    cargo's fingerprints do not name is kept."""
    deps, units, groups = os.path.join(profile_dir, "deps"), _units(profile_dir), {}
    for n in os.listdir(deps) if os.path.isdir(deps) else []:
        m, p = _HASHED.match(n), os.path.join(deps, n)
        if not m or m.group(2) not in units or os.path.islink(p) or not os.path.isfile(p) or not os.access(p, os.X_OK):
            continue
        groups.setdefault(units[m.group(2)], []).append((os.stat(p).st_mtime, p))
    return [p for g in groups.values() for _, p in sorted(g)[:-1]]


def _building(target, builds):
    """A cargo process writes into this target: one whose CARGO_TARGET_DIR names it, or one with none set
    working in its tree (main's checkout: anywhere under it but the worktrees, which are judged one by one).
    Only cargo is asked - every build, test run and link is under one, and its environment is readable
    (Apple's ld hides its own; the stage daemon's link step read as 'building main' - review, 2026-09-24)."""
    tree = os.path.dirname(target)
    for cwd, ctd in builds:
        if ctd:
            if os.path.realpath(ctd) == os.path.realpath(target):
                return True
        elif cwd == tree or (cwd.startswith(tree + "/") and
                             not (tree == REPO and cwd.startswith(os.path.join(REPO, ".claude", "worktrees") + "/"))):
            return True
    return False


def _builds():
    """(cwd, CARGO_TARGET_DIR or "") for every running cargo process (cargo, cargo-nextest, cargo-clippy...)."""
    out, pid, cwd = [], None, {}
    for l in sh(["lsof", "-a", "-c", "cargo", "-d", "cwd", "-Fpn"], timeout=60).splitlines():
        if l.startswith("p"):
            pid = l[1:]
        elif l.startswith("n") and pid:
            cwd[pid] = l[1:]
    for pid, d in cwd.items():
        env = sh(["ps", "eww", "-o", "command=", "-p", pid])
        m = re.search(r"(?:^|\s)CARGO_TARGET_DIR=(\S+)", env)
        out.append((d, m.group(1) if m else ""))
    return out


def sweep_superseded(dry):
    """User via supervisor, 2026-09-24 21:14 (Serves: cost - disk 303 -> 54 GB that day): superseded test
    executables piled up in main's target/, gate-target and every worker clone - 2365 of them, ~40 GB
    apparent, the SAME blocks in each (clones), so they free only when the last copy goes. After every
    landing (a new merge commit on main since the last sweep) and between gates, keep the newest executable
    per build unit in all of them in one pass. A target a cargo process writes into is skipped whole, and a
    file any process holds open is never touched."""
    if gate_running():
        return
    # A landing is a merge commit on main; the work runner's own board-sync commits move HEAD too, and are not one.
    head = sh(["git", "-C", REPO, "log", "-1", "--merges", "--format=%H"]).strip()
    mark = f"{S}/sweep-last"
    try:
        last = open(mark).read().strip()
    except OSError:
        last = ""
    if not head or head == last:
        return
    root = os.path.join(REPO, ".claude", "worktrees")
    targets = [(REPO, os.path.join(REPO, "target")), (None, os.path.join(S, "gate-target"))]
    targets += [(os.path.join(root, n), os.path.join(root, n, "target")) for n in sorted(os.listdir(root))] if os.path.isdir(root) else []
    # Where the compilers write: a target a build is writing into is left for the next landing.
    builds = _builds()
    before, swept, skipped, todo = disk_free_gb(), 0, [], []
    for wt, t in targets:
        prof = os.path.join(t, "debug")
        if os.path.islink(t) or not os.path.isdir(os.path.join(prof, "deps")):
            continue
        if _building(t, builds):
            skipped.append(os.path.basename(wt or t))
            continue
        todo += superseded_executables(prof)
    held = set()
    if todo:
        dirs = sorted({os.path.dirname(p) for p in todo if os.path.isdir(os.path.dirname(p))})
        # lsof exits 1 whether or not it found holders; a dir that vanished makes it print usage and
        # NOTHING - which would read as "nothing held". Anything on stderr aborts the sweep.
        r = subprocess.run(["lsof", "-Fn"] + [a for d in dirs for a in ("+d", d)], capture_output=True, text=True, timeout=120)
        if r.stderr.strip():
            log(f"SWEEP aborted: lsof over the deps dirs said {r.stderr.strip()[:160]!r}")
            return
        held = {l[1:] for l in r.stdout.splitlines() if l.startswith("n")}
    objs = {}
    for p in todo:
        if p in held:
            continue
        if not dry:
            d, base = os.path.dirname(p), os.path.basename(p)
            if d not in objs:
                objs[d] = [n for n in os.listdir(d) if n.endswith(".rcgu.o")]
            # split-debuginfo=unpacked leaves <exe>.<cgu>.rcgu.o beside it: orphans once the executable goes.
            for q in [p, p + ".d"] + [os.path.join(d, n) for n in objs[d] if n.startswith(base + ".")]:
                try:
                    os.remove(q)
                except FileNotFoundError:
                    pass
        swept += 1
    if not dry:
        with open(mark, "w") as f:
            f.write(head + "\n")
    if swept or skipped:
        log(f"{'DRY-RUN ' if dry else ''}SWEEP after {head[:8]}: {swept} superseded executable(s) removed across "
            f"{len(targets)} target(s), freed {disk_free_gb() - before:.1f} GB (df); skipped (building): {' '.join(skipped) or 'none'}")


def reclaim_idle_targets(claims, dry):
    """reap_worktrees keeps a worktree with unmerged commits or edits, and enqueue() frees a target only
    when its branch is queued - so a timed-out, blocked, conflicted or uncommitted worker keeps 4-8 GB
    of build output forever. 2026-09-24 09:47: 22 GB free (floor 20), 55 GB of it in twelve such idle
    targets, reclaimed by hand; on 2026-09-22 the floor held dispatch for 209 ticks. Remove target/ only
    (the source stays, a resume rebuilds through sccache) when no running claim owns the worktree, no
    process names it or sits in it, and nothing under target/ has been written for IDLE_TARGET_H."""
    root = os.path.join(REPO, ".claude", "worktrees")
    live = {c.get("wt") for c in claims.values() if c.get("state") == "running"}
    idle = []
    for name in sorted(os.listdir(root)) if os.path.isdir(root) else []:
        wt, t = os.path.join(root, name), os.path.join(root, name, "target")
        if wt in live or os.path.islink(wt) or os.path.islink(t) or not os.path.isdir(t):
            continue
        written = _target_written(t)
        if time.time() - written >= IDLE_TARGET_H * 3600:
            idle.append((wt, time.time() - written))
    if not idle:
        return
    # Coordinator-run work has no claim (09-24: t802/t803 e2e specs, t858); a process is the only sign.
    cwds = sh(["lsof", "-d", "cwd", "-Fn"], timeout=60)
    if not re.search(r"^p\d+", cwds, re.M):
        return   # lsof said nothing: no evidence the worktrees are unused, so no delete
    seen = sh(["ps", "-axo", "command"]) + "\n" + cwds
    for wt, age in idle:
        if re.search(re.escape(wt) + r"(/|\s|$)", seen, re.M):
            continue
        if dry:
            log(f"DRY-RUN would reclaim {wt}/target (idle {age / 3600:.1f} h)")
            continue
        # Renamed first: a build that starts during a long delete finds no target, never half of one.
        gone = os.path.join(wt, f"target.reclaim-{int(time.time())}")
        os.rename(os.path.join(wt, "target"), gone)
        shutil.rmtree(gone, ignore_errors=True)
        log(f"RECLAIM {wt}/target (idle {age / 3600:.1f} h, no running claim or process; source kept)")


# ---------- deflake dispatch (user, 2026-09-23) ----------
# "A red test that passes alone twice is accepted as a load flake and the batch lands; the 3rd flake
# of the same test within 7 days auto-spawns a deflaker, so flakes get fixed, not tolerated."
# py/hkpy/flakes.py keeps the ledger and appends one line per due test to deflake-requests.jsonl;
# this is the dispatch side. A request is not a board ticket, so its claim is keyed `DEFLAKE:<slug>`
# (never a T-id: sync_board, candidates and release_stale_claims all look claims up BY board id and
# so skip it) and carries `deflake: <slug>`, which routes it past every ticket-shaped path: its own
# reap, no result: block, no fix resume. One claim per slug, reused across runs: it remembers the
# newest request it consumed (`request_ts`), the run number, and when the last run ended.
DEFLAKE_PREFIX = "DEFLAKE:"


def deflake_slug(rid):
    """A request id -> the name its branch, worktree and work dir use. Lower-case [a-z0-9-] only, so
    an id the ledger builds from a test name can never make a bad ref or escape .claude/worktrees."""
    return re.sub(r"[^a-z0-9]+", "-", str(rid).lower()).strip("-")[:100]


def read_deflake_requests(path=None):
    """Valid requests, file order. Garbage (not JSON, not an object, no id/test, a non-numeric ts,
    an id with no usable characters) is skipped: the ledger is another process's output and one
    bad line must not stop every later request."""
    out = []
    try:
        lines = open(path or DEFLAKE_REQUESTS).read().splitlines()
    except OSError:
        return out
    for line in lines:
        try:
            r = json.loads(line)
        except ValueError:
            continue
        if not isinstance(r, dict) or not isinstance(r.get("id"), str) or not isinstance(r.get("test"), str) \
                or not r["test"].strip() or isinstance(r.get("ts"), bool) or not isinstance(r.get("ts"), (int, float)):
            continue
        slug = deflake_slug(r["id"])
        if slug:
            out.append(dict(r, slug=slug))
    return out


# A ticket branch still on its way to main - what a deflaker must not race (T-801, 2026-09-23 23:30:
# the runner dispatched deflakers on app-trace and fog-of-war while T-801's worker was rewriting both
# specs under the user's authorization; the coordinator had to hold one by hand).
_INFLIGHT = ("running", "queued", "review-failed", "gate-failed", "fix-held", "conflict")
_DEFER_SAID = set()


def deflake_deferred(slug, req, claims):
    """Why this deflake request must wait, or "". (a) its own last run left a branch with unmerged
    commits (held, blocked, review-failed): a second run would start over beside it; (b) an in-flight
    ticket branch edits the same spec file. Both are asked of merge_target(), not main: while a batch
    gates, main holds it provisionally and every branch in it reads as merged (09-24 09:39:52: a second
    app-trace deflaker went out 28 s after the batch carrying the first one's fix was committed)."""
    target = merge_target()
    c = claims.get(DEFLAKE_PREFIX + slug)
    if c and c.get("branch") and commits_ahead(c["branch"], target) > 0:
        return f"its branch {c['branch']} ({c.get('state')}) has unmerged commits"
    test = str(req.get("test", ""))
    if not test.endswith(".e2e.mjs"):
        return ""
    path = f"ui/e2e/{test}"
    # A `queued` claim counts only while its branch really is queued or gating: 15 claims were
    # `queued` 25-40 h after landing under another name (T-538 as task-t538-rl) - a forever wait.
    waiting = branches_waiting()
    for tid, t in claims.items():
        if tid.startswith(DEFLAKE_PREFIX) or t.get("state") not in _INFLIGHT or not t.get("branch"):
            continue
        if t["state"] == "queued" and t["branch"] not in waiting:
            continue
        if sh(["git", "diff", "--name-only", f"{target}...{t['branch']}", "--", path]).strip():
            return f"{tid}'s branch {t['branch']} ({t.get('state')}) edits {path}"
    return ""


def pending_deflakes(claims, reqs):
    """([(slug, newest unconsumed request)] that may dispatch now, oldest waiting first; changed).
    A request waits while the slug's previous run is still open or its branch unmerged (claim
    running - deflaker or review - or queued), logged once per request. Once that run has ended, a
    request whose ts <= the claim's `ended` is DROPPED: its evidence predates the last fix (a merged
    claim's `ended` is when release_stale_claims saw it land), and it is logged once and consumed."""
    by = {}
    for r in reqs:
        by.setdefault(r["slug"], []).append(r)
    ready, changed = [], False
    for slug, rs in by.items():
        c = claims.get(DEFLAKE_PREFIX + slug)
        consumed = c.get("request_ts", float("-inf")) if c else float("-inf")
        new = [r for r in rs if r["ts"] > consumed]
        if not new:
            continue
        latest = max(new, key=lambda r: r["ts"])
        if c and c.get("state") in ("running", "queued"):
            if c.get("wait_logged") != latest["ts"]:
                what = f"its {c.get('kind')} run is still running" if c["state"] == "running" else f"its branch {c.get('branch')} is not merged yet"
                log(f"DEFLAKE WAIT {slug}: request for {latest['test']} ({latest.get('count_7d')} in 7 d) - {what}")
                c["wait_logged"] = latest["ts"]
                changed = True
            continue
        if c:
            ended = c.get("ended") or c.get("started") or 0
            stale = [r for r in new if r["ts"] <= ended]
            if stale:
                c["request_ts"] = max(r["ts"] for r in stale)
                changed = True
                log(f"DEFLAKE DROP {slug}: {len(stale)} request(s) for {latest['test']} predate the last run's end "
                    f"({c.get('state')} at {time.strftime('%m-%d %H:%M', time.localtime(ended))}) - evidence from before the fix")
                new = [r for r in new if r["ts"] > ended]
                if not new:
                    continue
        why = deflake_deferred(slug, latest, claims)
        if why:
            if (slug, latest["ts"]) not in _DEFER_SAID:
                _DEFER_SAID.add((slug, latest["ts"]))
                log(f"DEFLAKE WAIT {slug}: request for {latest['test']} - {why}")
            continue
        ready.append((min(r["ts"] for r in new), slug, max(new, key=lambda r: r["ts"])))
    ready.sort(key=lambda x: x[0])
    return [(slug, r) for _, slug, r in ready], changed


def deflake_brief(key, req, wt, branch, d, base):
    incidents = "\n".join("  - " + json.dumps(i, sort_keys=True) for i in (req.get("incidents") or []) if isinstance(i, dict)) or "  (none listed)"
    how = ("cargo nextest run -p <crate> -E 'test(/<name>/)' (or -E 'binary(<file>)'), from the name above"
           if req.get("kind") == "rust" else
           "the one spec file alone, the way ui/e2e/run.mjs runs a single spec")
    return f"""You are a DEFLAKER for hackriff, dispatched by ops/work-runner.py because one test has now passed
alone after failing in a merge gate {req.get('count_7d', 3)} times within 7 days (user, 2026-09-23: the 3rd flake of a
test in 7 days auto-spawns a deflaker, so flakes get fixed, not tolerated). There is no coordinator in the loop:
read this fully and finish without asking questions.

TEST:   {req['test']}
KIND:   {req.get('kind', '?')}
COUNT:  {req.get('count_7d', '?')} in 7 days
REQUEST ID: {req['id']}
INCIDENTS (verbatim from the ledger):
{incidents}
EVIDENCE (verbatim from the ledger):
{req.get('evidence') or '(none given)'}

WHERE: your worktree is {wt} on branch {branch}, cut from {base} (the last gated main). Work ONLY there.
Never touch /Users/daniellewis/hackriff (the main checkout), never `git stash`, never `git reset --hard`,
never commit to main, never append to any merge queue - the runner does that after you hand back.
You do NOT edit docs/tasks.yaml (a hook denies it) and there is no ticket to update.

THE PROCEDURE (the deflake-triage skill, .claude/skills/deflake-triage/SKILL.md - read it; the rules below win
where they differ):
1. Run the test ALONE first, several times: {how}.
   Do NOT use `just test-one` (workspace-wide, 8+ minutes before it reaches a test).
2. FAILS ALONE = a REAL BUG, not a flake. It is not yours to paper over: hand back BLOCKED with the command, its
   exit code and the failing output, and what you believe the defect is. Do not change the assertion.
3. PASSES ALONE but failed in the gates = a load flake. Find the nondeterminism (wall-clock budgets instead of
   frames/events/sample index, ordering, shared ports/dirs/globals, a cold subprocess timed from the parent) and
   make the test DETERMINISTIC. NEVER a retry, NEVER a skip or #[ignore], NEVER a quarantine entry, NEVER a
   widened or shortened timeout, NEVER a deleted or weakened assertion.
4. PROVE IT: reintroduce the defect (or the timing it was sensitive to) and show the test goes RED; then GREEN with
   the fix. A green test that asserts nothing is worse than a flaky one. Record what was exercised.
5. Targeted tests only - the test itself and the crate or spec it lives in. Never the merge gate, the acceptance
   suite, the whole workspace or every spec (a hook blocks them). Never end a turn waiting on a background
   command. Run `just precheck <crates you touched>` before handing back if you touched Rust.
6. COMMIT everything on {branch}. The first line of the message starts "deflake: " and the message ends with
   Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
   An Opus reviewer reads the diff before the branch is queued for merge.

HAND BACK: your LAST step is to write this file, exactly this shape (JSON, no comments):
  {d}/handback.json
  {{"ticket": "{key}",
   "outcome": "done" | "blocked",
   "summary": "the nondeterminism found, the deterministic fix, the red-when-defect-returns proof, run counts",
   "commits": ["<short sha>", ...],
   "files": ["<path>", ...],
   "tests": [{{"cmd": "<exact command>", "exit": 0, "summary": "passed 20/20 alone; red with the defect back"}}, ...],
   "precheck": {{"exit": 0}},
   "blocked": {{"needs": "<if blocked: fails alone - the evidence; or what else unblocks it>"}},
   "observed_but_not_chased": ["<anything outside scope, with the exact evidence>", ...]}}
"done" is refused over a non-zero test exit - except a red-when-the-defect-returns proof, which you record
with "expect": "red" beside its non-zero "exit". Also end your final message with one line
HANDBACK: DONE   or   HANDBACK: BLOCKED <why>
Never exit with no commits and no hand-back file - that reads as a lost agent, not a finding.
"""


def launch_deflake(slug, req, prior, dry):
    """One deflaker run for `slug`: like launch() - worktree + branch, target clone inside the child,
    the same bound and env - but `--agent deflaker`, opus/high, cut from merge_target() (while a batch
    gates, main holds it ungated and may be rewound)."""
    key = DEFLAKE_PREFIX + slug
    run = (prior or {}).get("run", 0) + 1
    name = slug if run == 1 else f"{slug}-r{run}"      # a later run never reuses an earlier run's branch
    branch, wt, d = f"task-{name}", f"{REPO}/.claude/worktrees/{name}", f"{WORKDIR}/{name}"
    base = merge_target()
    if dry:
        log(f"DRY-RUN would dispatch deflaker {key} run {run} for {req['test']} [opus/high] -> {branch} from {base}")
        return None
    if sh(["git", "rev-parse", "--verify", "-q", branch]).strip():
        if not os.path.isdir(wt):
            sh(["git", "worktree", "add", wt, branch], check=True)
    else:
        sh(["git", "worktree", "add", wt, "-b", branch, base], check=True)
    os.makedirs(d, exist_ok=True)
    open(f"{d}/brief.md", "w").write(deflake_brief(key, req, wt, branch, d, base))
    open(f"{d}/request.json", "w").write(json.dumps(req, indent=1))
    cmd = ["claude", "-p", "--agent", "deflaker", "--model", "opus", "--effort", "high", "--dangerously-skip-permissions",
           "--output-format", "json", "--max-budget-usd", BUDGET_USD]
    clone = clone_cmd(wt)
    script = clone + "exec " + " ".join(f"'{a}'" for a in cmd) + f" < '{d}/brief.md'"
    env = dict(os.environ, **CARGO_ENV, HK_WORKER="1", HACKRIFF_OPS=S, **e2e_env(wt))
    p = subprocess.Popen(bounded(["bash", "-c", script]), cwd=wt, stdin=subprocess.DEVNULL,
                         stdout=open(f"{d}/out.json", "w"), stderr=open(f"{d}/run.log", "a"), env=env, start_new_session=True)
    log(f"DISPATCH {key} deflaker run {run} for {req['test']} ({req.get('count_7d')} in 7 d) [opus/high] pid={p.pid} -> {wt} from {base}")
    return {"ticket": key, "deflake": slug, "test": req["test"], "test_kind": req.get("kind"), "branch": branch, "wt": wt,
            "dir": d, "pid": p.pid, "started": time.time(), "model": "opus", "effort": "high", "group": None,
            "kind": "deflake", "review": True, "run": run, "request_ts": req["ts"], "base": base}


def dispatch_deflakes(claims, dry):
    """At most ONE deflaker per tick, as a worker under the dispatch cap, never through a hold."""
    reqs = read_deflake_requests()
    if not reqs:
        return False
    ready, changed = pending_deflakes(claims, reqs)
    if not ready:
        return changed
    if os.path.exists(f"{S}/dispatch-paused") or gate_holds_dispatch():
        return changed
    if busy_workers(claims) >= dispatch_cap() or disk_free_gb() < DISK_MIN_GB:
        return changed
    slug, req = ready[0]
    key = DEFLAKE_PREFIX + slug
    c = launch_deflake(slug, req, claims.get(key), dry)
    if c:
        claims[key] = dict(c, state="running")
        changed = True
    return changed


def reap_deflake(claims, key, c):
    """A finished deflaker (or its review). Commits and a clean tree -> the Opus review -> the queue;
    otherwise one attention line: DEFLAKE_NO_WORK / DEFLAKE_BLOCKED / DEFLAKE_ERROR / DEFLAKE_UNCOMMITTED."""
    d = c.get("dir") or f"{WORKDIR}/{c['deflake']}"
    now = time.time()
    if c["kind"] == "review":
        res = result_of(f"{d}/review.json")
        text = str(res.get("result", ""))
        c["ended"] = now
        if "VERDICT: PASS" in text:
            c["state"] = "queued"
            record_done(c, "review-pass", res)
            enqueue(c["branch"], c.get("wt"))
        else:
            fail = next((l for l in text.splitlines() if l.startswith("VERDICT: FAIL")), "no verdict line")
            c["state"] = "review-failed"
            record_done(c, "review-fail", res)
            attention(key, c["branch"], "DEFLAKE_REVIEW_FAIL", f"{fail[:200]} (full text: {d}/review.json)")
        return
    res = result_of(f"{d}/out.json")
    text = str(res.get("result", ""))
    if res.get("session_id"):
        c["session_id"] = res["session_id"]
    hb, hb_err = load_handback(d, key)
    if hb is None and hb_err and " names " in hb_err:
        hb, hb_err = load_handback(d, c["deflake"])         # the bare slug is an acceptable name too
    outcome, why = handback_outcome(hb, text)
    # A deflaker's red-when-the-defect-returns proof exits non-zero by design; it says so with
    # "expect": "red" (01:33 on 2026-09-24 a DONE deflake read as BLOCKED on exactly those runs).
    tests = [t for t in (hb or {}).get("tests", []) if isinstance(t, dict)]
    green = any(int(t.get("exit", 0) or 0) == 0 for t in tests)     # a red proof needs a green run beside it
    fails = [t for t in tests if int(t.get("exit", 0) or 0) != 0 and not (t.get("expect") == "red" and green)]
    if hb and outcome == "done" and fails:
        bad = fails[0]
        outcome, why = "blocked", f"claimed done with a failing test: {bad.get('cmd')} exit {bad.get('exit')}"
    if hb_err:
        log(f"HANDBACK {key}: {hb_err} ({outcome})")
    with open(f"{S}/handbacks.jsonl", "a") as f:
        f.write(json.dumps({"ts": int(now), "ticket": key, "kind": "deflake", "outcome": outcome, "briefed": True,
                            "how": "json" if hb else ("line" if "HANDBACK:" in text else "none")}) + "\n")
    ahead = commits_ahead(c["branch"], "main")
    dirty = [l for l in sh(["git", "status", "--porcelain"], cwd=c["wt"]).splitlines() if not l.startswith("??")] if os.path.isdir(c["wt"]) else []
    c["ended"] = now
    summary = (hb or {}).get("summary", "")
    if res.get("is_error"):
        c["state"] = "error"
        record_done(c, "error", res)
        attention(key, c["branch"], "DEFLAKE_ERROR", f"claude -p reported an error; see {d}/run.log")
    elif outcome in ("blocked", "cancel"):
        c["state"] = "blocked"
        record_done(c, "blocked", res)
        attention(key, c["branch"], "DEFLAKE_BLOCKED",
                  f"{c.get('test')}: {(why or '').strip()[:160]} | {summary.strip()[:300]} (ahead={ahead}; {d}/handback.json)")
    elif dirty:
        c["state"] = "uncommitted"
        record_done(c, "uncommitted", res)
        attention(key, c["branch"], "DEFLAKE_UNCOMMITTED", f"{len(dirty)} modified files left uncommitted in {c['wt']}; ahead={ahead}")
    elif ahead <= 0:
        c["state"] = "no-work"
        record_done(c, "no-work", res)
        attention(key, c["branch"], "DEFLAKE_NO_WORK", f"deflaker for {c.get('test')} exited with no commits; see {d}/out.json")
    else:
        record_done(c, "done-to-review", res)
        claims[key] = dict(launch_review(c), state="running")


_DEPTH_AT = [0.0]


def record_queue_depth():
    """One $HACKRIFF_OPS/queue-depth.jsonl line a minute: branches not yet on main (hkpy.flow's one
    definition) - sampled here because this runner ticks through a gate, the merge runner does not
    (user, 2026-09-24 17:02: 'merge queue is huge, is it growing? track its length on /flow')."""
    if time.time() - _DEPTH_AT[0] < 60:
        return
    _DEPTH_AT[0] = time.time()
    if f"{REPO}/py" not in sys.path:
        sys.path.append(f"{REPO}/py")
    from hkpy import flow
    d = flow.queue_waiting(S)
    with open(f"{S}/{flow.QUEUE_DEPTH_JSONL}", "a") as f:
        f.write(json.dumps({"ts": round(time.time(), 1), **{k: d[k] for k in ("waiting", "queued", "gating", "isolating")}}) + "\n")


def tick(dry):
    claims = load_claims()
    changed = reap(claims, dry)
    # A fix run that launch_fix HELD (dispatch-paused, gate-wanted, a gate in progress) is
    # relaunched once those clear - otherwise the claim sits as `fix-held`, the map shows the
    # ticket FAILED, and nothing ever moves it (T-513 sat that way from 20:20 to 00:20 on
    # 2026-09-22/23). Same holds as dispatch, checked here rather than trusted to be past.
    if not dry and not (os.path.exists(f"{S}/dispatch-paused") or gate_holds_dispatch()):
        for tid, c in list(claims.items()):
            if c.get("state") == "fix-held":
                log(f"FIX {tid}: hold cleared - relaunching the held fix ({(c.get('fail_line') or '')[:80]})")
                claims[tid] = launch_fix(dict(c, kind="work"), c.get("fail_line") or "held fix")
                changed = True

    try:
        changed |= release_stale_claims(claims, {t["id"]: t for t in board()})
    except Exception as e:
        log(f"release_stale_claims error: {e}")
    try:
        sync_board(claims, dry)
    except Exception as e:
        log(f"sync_board error: {e}")
    try:
        reap_worktrees(claims, dry)
    except Exception as e:
        log(f"reap_worktrees error: {e}")
    try:
        reclaim_e2e_data(dry)
    except Exception as e:
        log(f"reclaim_e2e_data error: {e}")
    try:
        reclaim_idle_targets(claims, dry)
    except Exception as e:
        log(f"reclaim_idle_targets error: {e}")
    try:
        sweep_superseded(dry)
    except Exception as e:
        log(f"sweep_superseded error: {e}")
    try:
        changed |= dispatch_deflakes(claims, dry)   # first: a flake that keeps costing gates outranks new work
    except Exception as e:
        log(f"dispatch_deflakes error: {e}")
    try:
        record_queue_depth()
    except Exception as e:
        log(f"record_queue_depth error: {e}")
    changed |= dispatch(claims, dry)
    if not dry:
        save_claims(claims)
    running = [c["ticket"] for c in claims.values() if c.get("state") == "running"]
    frontier = {}
    try:
        tasks = board(); by = {t["id"]: t for t in tasks}
        per_group = {}
        for c in claims.values():
            if c.get("state") == "running" and c.get("group"):
                per_group[c["group"]] = per_group.get(c["group"], 0) + 1
        held = {}
        for t in tasks:
            if t.get("status") != "todo" or t["id"] in claims:
                continue
            if t.get("needs") in ("user", "hardware") or t.get("blocked_on") or t.get("dispatch") == "manual":
                frontier["not_for_runner"] = frontier.get("not_for_runner", 0) + 1
            elif any(by.get(d, {}).get("status") not in ("done", "cancelled") for d in deps_of(t) if d in by):
                frontier["waiting_on_deps"] = frontier.get("waiting_on_deps", 0) + 1
            elif per_group.get(t.get("parallel_group"), 0) >= GROUP_CAP:
                frontier["held_by_group"] = frontier.get("held_by_group", 0) + 1
                held[t.get("parallel_group")] = held.get(t.get("parallel_group"), 0) + 1
            else:
                frontier["dispatchable"] = frontier.get("dispatchable", 0) + 1
        frontier["held_groups"] = held
    except Exception:
        pass
    status = {"tick": int(time.time()), "running": running, "frontier": frontier, "group_cap": GROUP_CAP, "budget": {"cores": CORES, "gate_reserve": GATE_RESERVE, "worker_cores": WORKER_CORES, "worker_jobs": WORKER_JOBS, "worker_test_threads": WORKER_TEST_THREADS}, "cap": CAP,
              "gate_running": gate_running(), "disk_free_gb": round(disk_free_gb()), "load1": round(os.getloadavg()[0], 1),
              "load_max": LOAD_MAX, "per_tick": PER_TICK,
              "queue_depth": queue_depth(), "queue_pause": QUEUE_PAUSE}
    json.dump(status, open(f"{S}/work-runner-status.json", "w"))
    return running


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--once", action="store_true")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--poll", type=int, default=30)
    ap.add_argument("--sync-board", action="store_true",
                    help="one board sync (sync_board) and exit - the merge runner calls it right after a landing, "
                         "the one moment it knows main is safe to commit")
    a = ap.parse_args()
    if a.sync_board:
        try:
            claims = json.load(open(CLAIMS)) if os.path.exists(CLAIMS) else {}
            sync_board(claims, False)
        except Exception as e:
            log(f"sync-board (from the merge runner) error: {e}")
        return
    os.makedirs(WORKDIR, exist_ok=True)
    for p in (NEEDS, DONE):
        open(p, "a").close()
    import launchpath
    launchpath.check(__file__, log)
    # Workers inherit this process's environment. Restarted from a role session, it carried that
    # session's HACKRIFF_ROLE into every worker, and ops/watchdog.py (which names a role session by
    # that variable before looking at claims) charged 5 workers' load to role:pipeline-manager -
    # 665 % and an over-budget alarm on 2026-09-24 03:20. The runner is no role: drop it.
    if os.environ.pop("HACKRIFF_ROLE", None):
        log("ENV: dropped an inherited HACKRIFF_ROLE - workers are owned by their claims, not by a role")
    same = subprocess.run(["git", "diff", "--quiet", "HEAD", "--", "ops/work-runner.py"], cwd=REPO).returncode == 0
    log(f"VERSION: {'matches' if same else 'DIFFERS FROM'} HEAD:ops/work-runner.py  ops={S} cap={CAP} dry={a.dry_run}")
    # What this process is actually running with - `just knobs show` reads it back as "effective".
    log(f"KNOBS: WORK_CAP={CAP} WORK_QUEUE_PAUSE={QUEUE_PAUSE} WORK_GATE_ALONE={int(GATE_ALONE)} WORK_PER_TICK={PER_TICK} "
        f"WORK_GATE_RESERVE={GATE_RESERVE} WORK_WORKER_CORES={WORKER_CORES} WORK_GROUP_CAP={GROUP_CAP} WORK_MAX_MINUTES={MAX_MINUTES}")
    while True:
        try:
            running = tick(a.dry_run)
            log(f"tick: {len(running)} running {running}")
        except Exception as e:
            log(f"tick error: {e}")
        if a.once:
            break
        time.sleep(a.poll)


if __name__ == "__main__":
    main()
