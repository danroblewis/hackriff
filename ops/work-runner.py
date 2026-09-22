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

# Agents are cheap while they think (~1% CPU each, measured 2026-09-22); builds are what saturate
# the 28 cores. So the ceiling is on AGENTS (8), and admission per tick is dynamic: the 1-minute
# load average must be under WORK_LOAD_MAX, disk over the floor, and a running gate counts as one
# builder. At most WORK_PER_TICK launches per tick so the load ramps instead of bursting.
CAP = int(os.environ.get("WORK_CAP", "8"))
LOAD_MAX = float(os.environ.get("WORK_LOAD_MAX", "20"))
PER_TICK = int(os.environ.get("WORK_PER_TICK", "2"))
# Tickets in one parallel_group share a crate, not necessarily a file. Serialising a whole group
# behind one ticket held 18 hk-pipeline tickets idle on 2026-09-22; a real conflict costs one
# re-merge (the merge runner skips the conflicting branch), so allow a few per group.
GROUP_CAP = int(os.environ.get("WORK_GROUP_CAP", "2"))
# THE GATE COMES FIRST. 2026-09-22 08:06-09:55: three docs-only branches (t800, t763, t299) each failed
# an individual gate on a different load-sensitive test while 6-8 workers built beside it at load ~17,
# and every failure costs a 50-minute isolation pass. So while a gate runs, admission drops to
# GATE_CAP workers and GATE_LOAD_MAX load; the original CLAUDE.md rule (4 builders INCLUDING the
# gate) was this, and raising the cap to 8 without it was the mistake.
GATE_CAP = int(os.environ.get("WORK_GATE_CAP", "5"))
GATE_LOAD_MAX = float(os.environ.get("WORK_GATE_LOAD_MAX", "18"))   # the gate alone runs this box at 8-13; workers are on E-cores meanwhile
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
CARGO_ENV = {"CARGO_BUILD_JOBS": "6", "CARGO_INCREMENTAL": "0", "CARGO_PROFILE_DEV_DEBUG": "line-tables-only"}


def log(msg):
    line = f"[{time.strftime('%m-%d %H:%M:%S')}] {msg}"
    print(line, file=sys.stderr)
    with open(LOG, "a") as f:
        f.write(line + "\n")


def attention(ticket, branch, kind, detail=""):
    with open(NEEDS, "a") as f:
        f.write(f"{time.strftime('%m-%d %H:%M')}  {branch}  {ticket}  {kind}  {detail}\n")
    log(f"ATTENTION {ticket} {kind} {detail}")


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
    return os.path.exists(BULKMARK) or os.path.exists(f"{REPO}/.git/MERGE_HEAD")


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
def brief_for(t, wt, branch):
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
Record your result on your branch with the task CLI, never by editing docs/tasks.yaml (a hook denies that):
write your report to a file, then `just task result {t['id']} --from <that file>` (what changed, test results,
anything you surfaced but correctly did not chase). `just task show {t['id']}` prints the ticket. Do NOT change its
status - the runner does.

TESTING PROTOCOL (CLAUDE.md): targeted tests only - `just test-crate <crate>`, `cargo nextest run -p <crate>
-E 'binary(<name>)'`, `just test-ui`. NEVER `just gate`, `just acceptance` or the full suite (a hook blocks
them). Never end a turn waiting on a background command; block on its output file instead.

FILING RULE (user, 2026-09-22): do not file new tickets for things you merely suspect. An OBSERVED failure
you cannot fix in scope goes in your result: text with the exact evidence; the coordinator decides.

HAND BACK: your final message must end with one line, exactly one of:
HANDBACK: DONE
HANDBACK: BLOCKED <one line: what specifically you need>

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
    # QoS: a worker runs at `utility` + nice 10 from birth, below the gate's default tier for CPU and
    # I/O. While a gate runs, apply_gate_qos() drops every worker to `background`, which on Apple
    # Silicon means the efficiency cores only - the 20 P-cores of this M3 Ultra belong to the gate.
    p = subprocess.Popen(["taskpolicy", "-c", "utility", "nice", "-n", "10", "bash", "-c", script], cwd=wt,
                         stdin=subprocess.DEVNULL, stdout=out, stderr=err, env=env, start_new_session=True)
    log(f"DISPATCH {tid} [{model}/{effort}] pid={p.pid} -> {wt} (target clone then exec claude; utility QoS)")
    return {"ticket": tid, "branch": branch, "wt": wt, "pid": p.pid, "started": time.time(), "model": model,
            "effort": effort, "group": t.get("parallel_group"), "milestone": t.get("milestone"), "kind": "work",
            "review": needs_review(t)}


def launch_review(claim):
    tid, branch, wt = claim["ticket"], claim["branch"], claim["wt"]
    d = f"{WORKDIR}/{tid}"
    prompt = f"""Review branch {branch} for hackriff before it is queued for merge. The diff is `git diff main...{branch}`
(run it from {wt}). The ticket text is in {d}/brief.md and the worker's report in {d}/out.json (field "result").
Check what the reviewer agent definition says to check, with CLAUDE.md's invariants and the thin-client rule.
Do not edit anything. Your final message must end with exactly one line:
VERDICT: PASS
VERDICT: FAIL <one line naming the defect and the file:line>
"""
    cmd = ["claude", "-p", "--agent", "reviewer", "--model", "opus", "--dangerously-skip-permissions",
           "--output-format", "json", "--max-budget-usd", BUDGET_USD]
    out = open(f"{d}/review.json", "w")
    err = open(f"{d}/run.log", "a")
    p = subprocess.Popen(cmd, cwd=wt, stdin=subprocess.PIPE, stdout=out, stderr=err, env=dict(os.environ, HACKRIFF_OPS=S),
                         start_new_session=True, text=True)
    p.stdin.write(prompt)
    p.stdin.close()
    log(f"REVIEW {tid} pid={p.pid}")
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


def record_done(claim, outcome, res):
    with open(DONE, "a") as f:
        f.write(json.dumps({"ticket": claim["ticket"], "branch": claim["branch"], "kind": claim.get("kind"),
                            "started": int(claim["started"]), "finished": int(time.time()),
                            "minutes": round((time.time() - claim["started"]) / 60, 1),
                            "cost_usd": res.get("total_cost_usd"), "turns": res.get("num_turns"),
                            "model": claim.get("model"), "outcome": outcome}) + "\n")


def enqueue(branch):
    lines = [l.strip() for l in open(MERGE_QUEUE)] if os.path.exists(MERGE_QUEUE) else []
    if branch not in lines:
        with open(MERGE_QUEUE, "a") as f:
            f.write(branch + "\n")
    log(f"QUEUED {branch} for merge")


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
                enqueue(c["branch"])
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
        ahead = int(sh(["git", "rev-list", "--count", f"main..{c['branch']}"]).strip() or 0)
        dirty = [l for l in sh(["git", "status", "--porcelain"], cwd=c["wt"]).splitlines() if not l.startswith("??")] if os.path.isdir(c["wt"]) else []
        if res.get("is_error"):
            c["state"] = "error"
            attention(tid, c["branch"], "ERROR", f"claude -p reported an error after {age_min:.0f} min; see {d}/run.log")
            record_done(c, "error", res)
        elif "HANDBACK: BLOCKED" in text:
            c["state"] = "blocked"
            why = next((l for l in text.splitlines() if l.startswith("HANDBACK: BLOCKED")), "")
            attention(tid, c["branch"], "BLOCKED", why[18:220])
            record_done(c, "blocked", res)
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
            enqueue(c["branch"])
    changed |= handle_gate_failures(claims, dry)
    return changed


def launch_fix(c, fail_line):
    tid, branch, wt = c["ticket"], c["branch"], c["wt"]
    d = f"{WORKDIR}/{tid}"
    n = c.get("fix_attempts", 0) + 1
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
Your final message must end with exactly one line: HANDBACK: DONE  or  HANDBACK: BLOCKED <why>
"""
    cmd = ["claude", "-p", "--resume", c["session_id"], "--model", c.get("model", "sonnet"), "--dangerously-skip-permissions",
           "--output-format", "json", "--max-budget-usd", BUDGET_USD]
    out_path = f"{d}/fix{n}.json"
    out = open(out_path, "w")
    err = open(f"{d}/run.log", "a")
    p = subprocess.Popen(cmd, cwd=wt, stdin=subprocess.PIPE, stdout=out, stderr=err,
                         env=dict(os.environ, **CARGO_ENV, HK_WORKER="1", HACKRIFF_OPS=S), start_new_session=True, text=True)
    p.stdin.write(prompt)
    p.stdin.close()
    log(f"FIX {tid} attempt {n}: resumed session {c['session_id'][:8]} pid={p.pid}")
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
    gate = gate_running()
    cap = min(CAP - 1, GATE_CAP) if gate else CAP
    free = cap - len(running)
    if free <= 0:
        return False
    if disk_free_gb() < DISK_MIN_GB:
        log(f"HOLD: {disk_free_gb():.0f} GB free < {DISK_MIN_GB} GB floor")
        return False
    load1 = os.getloadavg()[0]
    lmax = GATE_LOAD_MAX if gate else LOAD_MAX
    if load1 > lmax:
        log(f"HOLD: load {load1:.0f} > {lmax:.0f} ({len(running)} running{', gate running' if gate else ''})")
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
        r = subprocess.run(["git", "worktree", "remove", wt], cwd=REPO, capture_output=True, text=True)
        log(f"REAP {wt} ({branch}: {'merged' if merged else 'no commits'}) {'ok' if r.returncode == 0 else r.stderr.strip()[:120]}")


def apply_gate_qos(claims):
    """The macOS stand-in for a cgroup: while a gate runs, every worker process group goes to
    background QoS (E-cores only, throttled I/O); when it ends they come back to utility. Applied to
    every pid in the group each tick, so children spawned since are caught too."""
    gate = gate_running()
    for tid, c in claims.items():
        if c.get("state") != "running":
            continue
        pgid = c.get("pid")
        pids = subprocess.run(["pgrep", "-g", str(pgid)], capture_output=True, text=True).stdout.split()
        if not pids:
            continue
        want = "bg" if gate else "fg"
        if c.get("qos") == want and len(pids) == c.get("qos_n"):
            continue
        flag = "-b" if gate else "-B"
        for pid in pids:
            subprocess.run(["taskpolicy", flag, "-p", pid], capture_output=True)
        if c.get("qos") != want:
            log(f"QOS {tid}: {'background (E-cores) while the gate runs' if gate else 'restored to utility'} ({len(pids)} processes)")
        c["qos"] = want; c["qos_n"] = len(pids)


def tick(dry):
    claims = load_claims()
    changed = reap(claims, dry)
    try:
        apply_gate_qos(claims)
    except Exception as e:
        log(f"apply_gate_qos error: {e}")
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
    status = {"tick": int(time.time()), "running": running, "frontier": frontier, "group_cap": GROUP_CAP, "gate_cap": GATE_CAP, "gate_load_max": GATE_LOAD_MAX, "cap": (min(CAP - 1, GATE_CAP) if gate_running() else CAP),
              "gate_running": gate_running(), "disk_free_gb": round(disk_free_gb()), "load1": round(os.getloadavg()[0], 1),
              "load_max": LOAD_MAX, "per_tick": PER_TICK}
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
