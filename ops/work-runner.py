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
import os
import re
import shutil
import signal
import subprocess
import sys
import time

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

# THE RESOURCE MODEL IS A FIXED BUDGET, NOT A HEURISTIC (user, 2026-09-22). This box has 28 cores
# (M3 Ultra: 20 performance + 8 efficiency). The merge gate is reserved 14 (its 6 build jobs + 8 test
# threads, on P-cores, never contended). Each worker is BOUNDED, and the bound is inherited by its
# whole process tree: CARGO_BUILD_JOBS and NEXTEST_TEST_THREADS (environment - every rustc and test
# runner it spawns obeys them) plus a `taskpolicy -c background` QoS clamp at launch, which on Apple
# Silicon confines the tree to the efficiency cores. So a worker costs ~WORKER_CORES, the count is
# CAP, and the gate always has its reserve. A cpulimit FORK (see launch()) is the hard ceiling on
# top. No load-average admission, no gate-time throttling, no suspend/resume: a known bound per
# worker is the whole mechanism.
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
REAP_AFTER_MIN = int(os.environ.get("WORK_REAP_AFTER_MIN", "30"))   # a worktree younger than this is never reaped
MAX_MINUTES = int(os.environ.get("WORK_MAX_MINUTES", "180"))
REVIEW_MAX_MINUTES = int(os.environ.get("WORK_REVIEW_MAX_MINUTES", "45"))
# A branch that fails its merge gate goes back to the SAME worker: `claude -p --resume <session>`
# with the failure, so the agent that wrote the code fixes it with its context intact, instead of
# a fresh agent (or the coordinator) rediscovering everything. Capped like the merge runner's own
# attempts; the coordinator hears about it only when the cap is spent.
FIX_ATTEMPTS = int(os.environ.get("WORK_FIX_ATTEMPTS", "2"))
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


def log(msg):
    line = f"[{time.strftime('%m-%d %H:%M:%S')}] {msg}"
    print(line, file=sys.stderr)
    with open(LOG, "a") as f:
        f.write(line + "\n")


def attention(ticket, branch, kind, detail=""):
    with open(NEEDS, "a") as f:
        f.write(f"{time.strftime('%m-%d %H:%M')}  {branch}  {ticket}  {kind}  {detail}\n")
    log(f"ATTENTION {ticket} {kind} {detail}")
    # Discord (user, 2026-09-23): the kinds a person must act on are alerts too. ops/alert.py
    # dedupes per key and never raises; NO_WORK / UNCOMMITTED / CANCEL_PROPOSED are the
    # coordinator's routine and stay in the file only.
    level = {"BOARD_UNREADABLE": "red", "ERROR": "amber", "BLOCKED": "amber", "REVIEW_FAIL": "amber", "FIX_HELD": "info"}.get(kind)
    if level:
        try:
            subprocess.run([sys.executable, os.path.join(os.path.dirname(os.path.abspath(__file__)), "alert.py"),
                            level, f"{ticket} {kind}", f"{branch}: {detail[:300]}", "--key", f"wr:{ticket}:{kind}"],
                           capture_output=True, timeout=30)
        except Exception:
            pass
    # Poke the coordinator's pane the way the merge runner does; the file is the record, this is the wake-up.
    try:
        if subprocess.run(["tmux", "has-session", "-t", "dev"], capture_output=True).returncode == 0:
            subprocess.run(["tmux", "send-keys", "-t", "dev", "-l", f"WORK-RUNNER: {ticket} {kind} - {detail[:160]} See {NEEDS}."], capture_output=True)
            subprocess.run(["tmux", "send-keys", "-t", "dev", "Enter"], capture_output=True)
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
"done" if any test exit is non-zero. A CANCEL is yours to propose with evidence in the repo; an Opus review
confirms it before it lands. Also end your final message with one line `HANDBACK: <outcome>` as a fallback.
Never exit with no commits and no hand-back file - that reads as a lost agent, not a finding.

TICKET:
{body}
"""


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
    cmd = ["claude", "-p", "--agent", "worker", "--model", model, "--dangerously-skip-permissions",
           "--output-format", "json", "--max-budget-usd", BUDGET_USD]
    if effort in EFFORTS:
        cmd += ["--effort", effort]
    # The build-target clone (`cp -c`, an APFS clone) walks main's whole target tree and takes
    # minutes; done inline it blocked every tick for that long (2026-09-22: two dispatches took
    # eight minutes of a tick). So the clone runs INSIDE the worker's own process, which then
    # `exec`s claude under the same pid - the claim's pid is valid from the first second, reap sees
    # it alive through both phases, and the tick returns at once. The brief is read from its file.
    clone = f'[ -d "{REPO}/target" ] && [ ! -e "{wt}/target" ] && cp -c -R -p "{REPO}/target" "{wt}/target"; '
    script = clone + "exec " + " ".join(f"'{a}'" for a in cmd) + f" < '{d}/brief.md'"
    env = dict(os.environ, **CARGO_ENV, HK_WORKER="1", HACKRIFF_OPS=S)
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
            "review": needs_review(t)}


def launch_review(claim):
    tid, branch, wt = claim["ticket"], claim["branch"], claim["wt"]
    d = f"{WORKDIR}/{tid}"
    cancel = c_reason = claim.get("cancel_reason")
    extra = (f"\nTHIS BRANCH CANCELS THE TICKET. The worker's reason: {cancel}\nYour job is to verify that reason against the repo "
             "(is the work really done at the commit named? is the decision real and does it obsolete THIS ticket?). "
             "PASS only if the evidence holds; FAIL names what is missing.\n") if cancel else ""
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
    with open(DONE, "a") as f:
        f.write(json.dumps({"ticket": claim["ticket"], "branch": claim["branch"], "kind": claim.get("kind"),
                            "started": int(claim["started"]), "finished": int(time.time()),
                            "minutes": round((time.time() - claim["started"]) / 60, 1),
                            "cost_usd": res.get("total_cost_usd"), "turns": res.get("num_turns"),
                            "model": claim.get("model"), "outcome": outcome}) + "\n")


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


def reap(claims, dry):
    changed = False
    for tid, c in list(claims.items()):
        if c.get("state") != "running":
            continue
        pid = c["pid"]
        age_min = (time.time() - c["started"]) / 60
        limit = REVIEW_MAX_MINUTES if c["kind"] == "review" else MAX_MINUTES
        if alive(pid):
            if age_min > limit:
                try:
                    os.killpg(pid, signal.SIGTERM)
                except OSError:
                    pass
                c["state"] = "timeout"
                attention(tid, c["branch"], "TIMEOUT", f"{c['kind']} exceeded {limit} min; killed; worktree kept")
                record_done(c, "timeout", {})
                changed = True
            continue
        # finished
        changed = True
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
        if hb and outcome == "done" and any(int(t.get("exit", 0) or 0) != 0 for t in hb.get("tests", []) if isinstance(t, dict)):
            bad = next(t for t in hb["tests"] if int(t.get("exit", 0) or 0) != 0)
            outcome, why = "blocked", f"claimed done with a failing test: {bad.get('cmd')} exit {bad.get('exit')}"
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
        if hb and outcome in ("done", "cancel") and ahead > 0 and not dirty:
            write_result(c, hb)                    # the board line the worker used to write by hand
            ahead = int(sh(["git", "rev-list", "--count", f"main..{c['branch']}"]).strip() or 0)
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
    changed |= handle_gate_failures(claims, dry)
    return changed


def launch_fix(c, fail_line):
    tid, branch, wt = c["ticket"], c["branch"], c["wt"]
    d = f"{WORKDIR}/{tid}"
    n = c.get("fix_attempts", 0) + 1
    # A fix run is a dispatch. It used to bypass every hold: at 15:19 on 2026-09-22, with
    # dispatch-paused in force and the box meant to be empty for the gate, a GATE_FAIL on
    # task-t700 resumed a worker to "fix" a defect that was main's, not the branch's.
    if os.path.exists(f"{S}/dispatch-paused") or os.path.exists(f"{S}/gate-wanted") or gate_running():
        attention(tid, branch, "FIX_HELD", f"fix attempt {n} NOT launched: dispatch is paused/gate pending ({fail_line[:160]})")
        return dict(c, state="fix-held", fail_line=fail_line[:300])
    kind = "its REVIEW" if fail_line.startswith("REVIEW_FAIL") else "its merge gate on main"
    prompt = f"""Your branch {branch} FAILED {kind} (fix attempt {n} of {FIX_ATTEMPTS}). The finding:
{fail_line}
If this is a review finding, fix exactly what it names (the reviewer's full text is in the file it cites), then
re-run the targeted tests and hand back; the branch is reviewed again before it is queued.
The full gate log is {MERGE_LOG}; find your run with `grep -n 'GATE FAILED {branch}\\|FAIL \\[\\|FAILED just\\|error\\[' {MERGE_LOG} | tail -40`.
TRIAGE FIRST, in your worktree {wt}: merge main in (`git merge main`), then run the failing test ALONE
(`cargo nextest run -p <crate> -E 'test(/<name>/)'` or `just test-ui`). Fails alone = a real bug: fix it.
Passes alone but failed in the gate = load-sensitive: make it deterministic (never a retry, never a skip).
If the failure is in code you did not touch and is a known bug on main, say so precisely and hand back BLOCKED.
Then: targeted tests, `just precheck <crates>`, commit on {branch}, `just task note {tid} --text "<what the gate found and what you changed>"`.
Same rules as before: never touch the main checkout, never the full gate, never edit docs/tasks.yaml by hand.
When finished, REWRITE {d}/handback.json (same shape as before: outcome done|blocked, summary, commits, tests) and end
your final message with one line HANDBACK: DONE or HANDBACK: BLOCKED <why>.
"""
    cmd = ["claude", "-p", "--resume", c["session_id"], "--model", c.get("model", "sonnet"), "--dangerously-skip-permissions",
           "--output-format", "json", "--max-budget-usd", BUDGET_USD]
    out_path = f"{d}/fix{n}.json"
    out = open(out_path, "w")
    err = open(f"{d}/run.log", "a")
    p = subprocess.Popen(bounded(cmd), cwd=wt, stdin=subprocess.PIPE, stdout=out, stderr=err,
                         env=dict(os.environ, **CARGO_ENV, HK_WORKER="1", HACKRIFF_OPS=S), start_new_session=True, text=True)
    p.stdin.write(prompt)
    p.stdin.close()
    log(f"FIX {tid} attempt {n}: resumed session {c['session_id'][:8]} pid={p.pid} (bounded)")
    return dict(c, pid=p.pid, started=time.time(), kind="fix", state="running", out=out_path, fix_attempts=n)


def release_stale_claims(claims, tasks_by_id):
    changed = False
    for tid, c in list(claims.items()):
        if c.get("state") in ("no-work", "error", "timeout") and time.time() - c.get("started", 0) > RELEASE_AFTER_H * 3600:
            if tasks_by_id.get(tid, {}).get("status") == "todo":
                log(f"RELEASE {tid}: claim ended {c['state']} {RELEASE_AFTER_H:.0f}h+ ago and the ticket is still todo - eligible again")
                del claims[tid]; changed = True
    return changed


def handle_gate_failures(claims, dry):
    """Merge-runner GATE_FAIL lines for branches this runner queued -> resume the worker to fix.
    Also: claims left in review-failed (from before the review-fix path existed) get the same path."""
    for tid, c in list(claims.items()):
        if c.get("state") == "review-failed" and c.get("session_id") and c.get("fix_attempts", 0) < FIX_ATTEMPTS and os.path.isdir(c.get("wt", "")):
            try:
                text = str(result_of(f"{WORKDIR}/{tid}/review.json").get("result", ""))
                fail = next((l for l in text.splitlines() if l.startswith("VERDICT: FAIL")), "VERDICT: FAIL (see review.json)")
            except Exception:
                fail = "VERDICT: FAIL (see review.json)"
            if not dry:
                claims[tid] = launch_fix(dict(c, kind="work"), f"REVIEW_FAIL {fail[:300]} (full review: {WORKDIR}/{tid}/review.json)")
    try:
        lines = [l.rstrip("\n") for l in open(MERGE_NEEDS) if "GATE_FAIL" in l]
    except FileNotFoundError:
        return False
    by_branch = {c["branch"]: tid for tid, c in claims.items() if c.get("branch")}
    changed = False
    for line in lines:
        parts = line.split()
        branch = parts[2] if len(parts) > 2 else ""
        tid = by_branch.get(branch)
        if not tid:
            continue
        c = claims[tid]
        if line in c.get("gate_fails_seen", []) or c.get("state") != "queued":
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
def sync_board(claims, dry):
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
        sh(["git", "checkout", "--", "docs/tasks.yaml"])
        attention("board", "main", "SYNC_YAML_BROKEN", str(e)[:200])
        return
    if gate_running():  # re-check right before committing: a bulk may have started during the edit
        sh(["git", "checkout", "--", "docs/tasks.yaml"])
        return
    sh(["git", "add", "docs/tasks.yaml"])
    msg = "Board: work-runner status sync - " + ", ".join(f"{a} {b}" for a, b, _ in flips)
    r = subprocess.run(["git", "commit", "-q", "-m", msg], cwd=REPO, capture_output=True, text=True)
    if r.returncode != 0:
        sh(["git", "checkout", "--", "docs/tasks.yaml"])
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


def dispatch(claims, dry):
    running = [c for c in claims.values() if c.get("state") == "running" and c.get("kind") == "work"]
    cap = CAP
    free = cap - len(running)
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
    if gate_running():
        log(f"HOLD: a gate is running ({len(running)} workers still finishing)")
        return False
    depth = queue_depth()
    if depth >= QUEUE_PAUSE:
        log(f"HOLD: {depth} branches queued for merge >= {QUEUE_PAUSE}; letting {len(running)} workers drain so the gate can run alone")
        return False
    # The gate is IMMINENT when something is queued and no worker is running: the merge runner
    # starts it within seconds, and its bulk marker can land a tick after this check (21:02:21
    # marker vs 21:02:22 dispatch on 2026-09-22 - two workers built beside that gate). Do not
    # dispatch into that window; the gate takes the batch, then dispatch resumes.
    if depth > 0 and not running:
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
        dirty = any(not l.startswith("??") for l in sh(["git", "status", "--porcelain"], cwd=wt).splitlines())
        if dirty:
            continue
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
        force = merged or claim.get("state") in ("no-work", "error", "timeout")
        r = subprocess.run(["git", "worktree", "remove"] + (["--force"] if force else []) + [wt], cwd=REPO, capture_output=True, text=True)
        log(f"REAP {wt} ({branch}: {'merged' if merged else 'no commits'}{', forced' if force else ''}) {'ok' if r.returncode == 0 else r.stderr.strip()[:120]}")




def tick(dry):
    claims = load_claims()
    changed = reap(claims, dry)
    # A fix run that launch_fix HELD (dispatch-paused, gate-wanted, a gate in progress) is
    # relaunched once those clear - otherwise the claim sits as `fix-held`, the map shows the
    # ticket FAILED, and nothing ever moves it (T-513 sat that way from 20:20 to 00:20 on
    # 2026-09-22/23). Same holds as dispatch, checked here rather than trusted to be past.
    if not dry and not (os.path.exists(f"{S}/dispatch-paused") or os.path.exists(f"{S}/gate-wanted") or gate_running()):
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
    a = ap.parse_args()
    os.makedirs(WORKDIR, exist_ok=True)
    for p in (NEEDS, DONE):
        open(p, "a").close()
    same = subprocess.run(["git", "diff", "--quiet", "HEAD", "--", "ops/work-runner.py"], cwd=REPO).returncode == 0
    log(f"VERSION: {'matches' if same else 'DIFFERS FROM'} HEAD:ops/work-runner.py  ops={S} cap={CAP} dry={a.dry_run}")
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
