#!/usr/bin/env bash
# Deterministic merge runner — NO AI in the happy path.
#
# The coordinator appends READY branch names (one per line, in dependency order)
# to merge-queue.txt when a ticket is code-complete. This runner then, per branch:
#   git merge --no-ff --no-commit <branch>  →  just gate-merge  →
#   on PASS: commit the merge + remove the worktree.
#   on CONFLICT or GATE FAILURE: abort + append to merge-needs-attention.txt (AI handles it).
# It is the SOLE merger to main. AI is only needed for the exceptions.
set -uo pipefail
REPO=/Users/daniellewis/hackriff
S="${HACKRIFF_OPS:-$HOME/.hackriff-ops}"; mkdir -p "$S"
# T-761: SAY WHERE THE OPS STATE IS, at a location everyone can find without being told.
#
# `HACKRIFF_OPS` is per-process, so the runner and the coordinator can disagree about where the
# queue, the log and the staged-bulk marker live — and on 2026-09-22 they did, four times: two
# branches were stranded because a queue append went to a file the runner never read, a stale
# entry sat in the other copy, and — worst — T-650's `bulk-in-progress` marker was INVISIBLE to
# the coordinator's `just reconcile`, which is the one reader it was written for. A warning nobody
# receives is the false-quiet the marker exists to remove, reproduced one level up.
#
# So the runner publishes its own choice at the FIXED default path. Anything that needs the ops
# state reads this pointer instead of guessing, and a session with no `HACKRIFF_OPS` at all lands
# in the right directory. The pointer is written on every start, so a restart under a different
# env corrects it rather than leaving a stale claim.
mkdir -p "$HOME/.hackriff-ops"
printf '%s\n' "$S" > "$HOME/.hackriff-ops/active-ops-dir"
QUEUE=$S/merge-queue.txt
NEEDS=$S/merge-needs-attention.txt
DONELOG=$S/merge-done.txt
LOG=$S/merge-runner.log
# T-534: per-branch gate-attempt ledger, "<branch> <tip-sha> <attempts>" one per line.
ATTEMPTS=$S/merge-attempts.txt
# T-582 follow-on: a bulk COMMITS EACH MERGE AS IT GOES and only rewinds if the gate fails, so for
# the 15-25 minutes a bulk gate runs, `main` carries commits that HAVE NOT PASSED A GATE and may be
# reset away. Anything sampling main in that window - a reconcile, an `ahead` count, a fresh branch
# cut from main - is reading a PROVISIONAL state. That cost three separate incidents on 2026-09-21:
# a branch deleted on ahead=0 that had to be recovered from a merge commit's second parent, a batch
# announced as landed from `git log --merges` that the gate then rewound, and a worker branch cut
# from staged-but-ungated main that silently absorbed three other tickets' work.
# So the bulk declares itself. The file exists ONLY while main is provisional.
BULKMARK=$S/bulk-in-progress
# T-543: one JSON line per LANDED ticket - {ticket, branch, first_commit_ts, merge_ts,
# land_minutes, gate_attempts}. `merge-done.txt` records THAT a branch merged; this records
# what it COST, which is the number T-543 exists to watch. Written next to the gate's own
# per-run timings ($HACKRIFF_OPS/gate-timings.jsonl) so `just cycle-time` reads one directory.
# No database and no daemon: append-only text, and `gate_attempts` is read from the ledger
# this script already keeps rather than counted a second way.
LANDED=$S/landed.jsonl
# THE KNOB STORE (pipeline manager, 2026-09-23): `$S/env` holds KEY=VALUE lines written by
# `just knobs set`, so an experiment's setting survives a plain restart (on 2026-09-23 the cap-6
# trial lived only in one process's environment). The process environment still wins - a
# deliberate one-off override on the command line is not silently replaced by the store.
if [ -f "$S/env" ]; then
  while IFS='=' read -r k v; do
    case "$k" in ''|'#'*) continue ;; esac
    [ -z "${!k+x}" ] && export "$k=$v"
  done < "$S/env"
fi
MAX_ATTEMPTS=${MAX_ATTEMPTS:-2}
# Most branches one batch may carry (user, 2026-09-22); the rest keep their queue order.
BULK_MAX=${BULK_MAX:-15}
DRY_RUN=${DRY_RUN:-0}
touch "$QUEUE" "$NEEDS" "$DONELOG" "$ATTEMPTS" "$LANDED"

# Log lines go to STDERR, never stdout. `ready_filter` runs inside `ready=$(ready_filter ...)`,
# so anything it prints on stdout is read back as a BRANCH NAME. On 2026-09-21 that turned
# "SKIP task-t307: nothing ahead of main (already merged?)" into the branches `ahead`, `of`,
# `(already` and `merged?)`, reported a 7-branch batch as "BULK attempt (36)", and wrote those
# words into merge-needs-attention.txt as branches needing a person. Harmless only by luck - the
# bogus names failed the rev-parse check. Same family as the `tr -d` bug that once glued every
# queued branch into one unmergeable token: a helper's diagnostics leaking into its data.
log(){ echo "[$(date '+%m-%d %H:%M:%S')] $*" | tee -a "$LOG" >&2; }
# Discord (user, 2026-09-23): every exception the runner hands to a person is also an alert;
# every landing is a green one-liner. ops/alert.py dedupes by key and never fails the caller.
alert(){ python3 "$(dirname "${BASH_SOURCE[0]}")/alert.py" "$@" >/dev/null 2>&1 || true; }
# The identity of a batch for the suite-broken hold: branch NAMES AND TIPS, sorted. A hold keyed
# on names alone never released on a fix pushed to a queued branch - at 04:06 on 2026-09-23 the
# fixed branch sat behind "waiting for the queue to change" until a person deleted the marker.
batch_sig(){ for b in "$@"; do printf '%s@%s\n' "$b" "$(git -C "$REPO" rev-parse --short "$b" 2>/dev/null)"; done | sort | tr '\n' ' '; }
notify_coordinator(){
  alert amber "merge runner needs a person" "$1" --key "mr:$(echo "$1" | cut -c1-48)"
  tmux has-session -t dev 2>/dev/null || return 0; tmux send-keys -t dev -l "MERGE-RUNNER: $1 See $NEEDS; fix it, then re-queue the branch." 2>/dev/null; sleep 1; tmux send-keys -t dev Enter 2>/dev/null; }
# Edge-triggered wake on a SUCCESSFUL merge: a clean merge drains the queue and may unblock
# dependent tickets, but nothing else pings the coordinator for it (task-completions and the
# failure ping above cover their cases). Without this, the coordinator can sit idle after a
# green merge with startable work undone. It says "reconcile", never a computed to-do list:
# the coordinator's `just reconcile` is the single source of truth, and any list we pasted here
# would be stale by the time it acts.
notify_ok(){
  alert green "landed" "$1"
  tmux has-session -t dev 2>/dev/null || return 0; tmux send-keys -t dev -l "MERGE-RUNNER: $1 Reconcile, then fill the builder cap from startable work." 2>/dev/null; sleep 1; tmux send-keys -t dev Enter 2>/dev/null; }
ticket_of(){ echo "$1" | sed -E 's/^task-t0*([0-9]+)$/T-\1/I'; }
worktree_of(){ git -C "$REPO" worktree list --porcelain \
  | awk -v b="refs/heads/$1" '/^worktree /{p=substr($0,10)} /^branch /{if(substr($0,8)==b) print p}'; }

# --- gate-attempt ledger (T-534) -------------------------------------------
# A branch that failed its gate and has NOT been touched since will fail the same
# way: re-gating it costs 20 minutes and teaches nothing. So record the tip that
# failed, and refuse to spend a gate on that same tip twice. After MAX_ATTEMPTS
# distinct attempts on one branch, stop retrying entirely and say so loudly —
# repeated automatic retries hide a branch that needs a person.
attempt_line(){ grep -E "^$1 " "$ATTEMPTS" 2>/dev/null | tail -1; }
attempts_of(){ local l; l=$(attempt_line "$1"); [ -n "$l" ] && echo "$l" | awk '{print $3}' || echo 0; }
failed_sha_of(){ local l; l=$(attempt_line "$1"); [ -n "$l" ] && echo "$l" | awk '{print $2}' || echo ""; }
record_attempt(){ # branch sha
  local n; n=$(attempts_of "$1"); n=$((n+1))
  grep -vE "^$1 " "$ATTEMPTS" > "$ATTEMPTS.tmp" 2>/dev/null || true
  printf '%s %s %s\n' "$1" "$2" "$n" >> "$ATTEMPTS.tmp"; mv "$ATTEMPTS.tmp" "$ATTEMPTS"
}
clear_attempts(){ grep -vE "^$1 " "$ATTEMPTS" > "$ATTEMPTS.tmp" 2>/dev/null || true; mv "$ATTEMPTS.tmp" "$ATTEMPTS"; }

# T-543: record what a landed ticket cost. Called BEFORE clear_attempts, so gate_attempts is
# the count that branch actually spent. first_commit_ts comes from the merge commit's second
# parent (the branch tip), which survives the branch and its worktree being deleted - the
# reason `just cycle-time` could not report commit->merge for already-merged work until now.
record_landed(){ # branch
  local b=$1 t merge_sha first now land
  t=$(ticket_of "$b")
  # The merge commit NAMING THIS BRANCH, not HEAD: in a bulk batch HEAD is the last merge,
  # so HEAD^2 would attribute every branch's commits to the last one merged.
  merge_sha=$(git -C "$REPO" log HEAD --merges --format=%H --fixed-strings --grep "$b" -n 1 2>/dev/null)
  [ -z "$merge_sha" ] && merge_sha=$(git -C "$REPO" rev-parse HEAD 2>/dev/null)
  first=$(git -C "$REPO" log --format=%ct "${merge_sha}^1..${merge_sha}^2" 2>/dev/null | tail -1)
  now=$(date +%s)
  if [ -n "${first:-}" ]; then land=$(( (now - first) / 60 )); else first=null; land=null; fi
  printf '{"ticket":"%s","branch":"%s","first_commit_ts":%s,"merge_ts":%s,"land_minutes":%s,"gate_attempts":%s,"merge":"%s"}\n' \
    "$t" "$b" "$first" "$now" "$land" "$(( $(attempts_of "$b") + 1 ))" "$merge_sha" >> "$LANDED"
  flip_done "$t" "$merge_sha"
}

# The board flip belongs HERE, at the instant of landing, because this runner is the one process
# allowed to commit to main right now. A bystander waiting for a "safe" moment never finds one: on
# 2026-09-22 the work runner's board sync committed ZERO times in four hours of back-to-back gates,
# so the burndown showed six landed branches as still open. Uses the task CLI when main has it
# (py/hkpy/tasks.py, T-taskcli); a ticket-less branch (task-guards) has nothing to flip.
flip_done(){ # ticket merge_sha
  local t=$1 sha=$2
  case "$t" in T-*) ;; *) return 0 ;; esac
  [ -f "$REPO/py/hkpy/tasks.py" ] || { log "flip_done $t: no task CLI on main yet - the board keeps todo until reconcile"; return 0; }
  if (cd "$REPO" && uv run --locked --project py python -m hkpy.tasks set "$t" status=done commit="${sha:0:8}" >>"$LOG" 2>&1 \
      && git add docs/tasks.yaml && git commit -q -m "Board: $t landed as ${sha:0:8} (merge runner)"); then
    log "BOARD $t -> done (${sha:0:8})"
  else
    (cd "$REPO" && git checkout -q -- docs/tasks.yaml 2>/dev/null)
    log "flip_done $t FAILED - board left as is; needs reconcile"
  fi
}

# returns: 0 = handled (merged/skipped/flagged), 1 = transient (requeue + wait)
process(){
  local branch=$1 ticket; ticket=$(ticket_of "$branch")
  cd "$REPO" || return 1
  git rev-parse --verify "$branch" >/dev/null 2>&1 || { log "SKIP $branch: no such branch"; return 0; }
  [ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || { log "WAIT $branch: HEAD not on main"; return 1; }
  [ -e "$REPO/.git/MERGE_HEAD" ] && { log "WAIT $branch: a merge is already in progress"; return 1; }
  git diff --quiet && git diff --cached --quiet || { log "WAIT $branch: main tree dirty (coordinator mid-commit)"; return 1; }
  local ahead; ahead=$(git rev-list --count "main..$branch" 2>/dev/null || echo 0)
  if [ "${ahead:-0}" -eq 0 ]; then log "SKIP $branch: nothing ahead of main (already merged?)"; return 0; fi

  local tip prev tries; tip=$(git rev-parse "$branch"); prev=$(failed_sha_of "$branch"); tries=$(attempts_of "$branch")
  if [ -n "$prev" ] && [ "$prev" = "$tip" ]; then
    log "SKIP $branch: UNCHANGED SINCE ITS GATE FAILURE ($tip) - needs a fix, not a re-queue"
    echo "$(date '+%m-%d %H:%M')  $branch  $ticket  UNCHANGED_SINCE_FAIL ($tip)" >> "$NEEDS"
    notify_coordinator "$ticket ($branch) was re-queued UNCHANGED since its gate failure - fix the branch first; it was NOT re-gated."
    return 0
  fi
  if [ "${tries:-0}" -ge "$MAX_ATTEMPTS" ]; then
    log "GIVE UP $branch: $tries gate attempts already (cap $MAX_ATTEMPTS) - escalating, no further automatic retries"
    echo "$(date '+%m-%d %H:%M')  $branch  $ticket  GAVE_UP after $tries attempts - NEEDS A PERSON" >> "$NEEDS"
    notify_coordinator "$ticket ($branch) has now FAILED $tries gate attempts; the runner has GIVEN UP and will not retry it."
    return 0
  fi
  if [ "$DRY_RUN" = "1" ]; then log "DRY-RUN would merge $branch ($ticket, $ahead ahead)"; return 0; fi

  log "MERGE start $branch ($ticket, $ahead commits ahead)"
  if ! git merge --no-ff --no-commit "$branch" >>"$LOG" 2>&1; then
    git merge --abort 2>/dev/null || true
    log "CONFLICT $branch -> flag for AI"
    echo "$(date '+%m-%d %H:%M')  $branch  $ticket  CONFLICT" >> "$NEEDS"; notify_coordinator "$ticket ($branch) hit a MERGE CONFLICT with main."; return 0
  fi
  log "GATE $branch (just gate-merge; may take 15-25 min)…"
  local gate_line rc; gate_line=$(( $(wc -l < "$LOG") ))
  limited just gate-merge; rc=$?
  # Same triage as a bulk (flake_retry): a single branch's red used to go straight to
  # GATE_FAIL and burn one of its MAX_ATTEMPTS on a load flake it never touched - task-gatefix
  # spent its second and last attempt that way on 2026-09-22 (api_contract tile_shadow…).
  if [ "$rc" -ne 0 ]; then flake_retry "" "$gate_line" "$ticket" "just gate-merge"; rc=$?; fi
  if [ "$rc" -eq 0 ]; then
    # T-840: THE STAGED MERGE MUST STILL BE THE ONE WE GATED.
    #
    # `git commit` with no MERGE_HEAD writes an ORDINARY commit of whatever is in the index. On
    # 2026-09-22 a `git stash` in another session dropped MERGE_HEAD mid-gate, and this line
    # committed `ea91c27c`: a SINGLE-PARENT commit carrying 4 of the branch's 44 files. It looked
    # like a merge in the log, the gate had passed, and nothing said otherwise - the branch read as
    # landed while most of its work was not on main.
    #
    # The runner already refuses to START a merge when MERGE_HEAD is present; this is the symmetric
    # check at the other end, and it fails CLOSED: if the state is not exactly what we gated, do not
    # commit, leave main untouched, and hand it to a person. A wrong merge is far worse than a
    # delayed one.
    local staged_head branch_tip
    staged_head=$(git -C "$REPO" rev-parse --verify --quiet MERGE_HEAD || true)
    branch_tip=$(git -C "$REPO" rev-parse --verify --quiet "$branch" || true)
    if [ -z "$staged_head" ] || [ "$staged_head" != "$branch_tip" ]; then
      # Two cases, neither a person's job. (a) The branch moved while its old tip was gated
      # (a fix pushed mid-gate, 2026-09-22 22:02): the staged merge is of a tip nobody wants
      # any more - abort it and re-queue the branch, which gates the new tip. (b) MERGE_HEAD is
      # gone (someone stashed/reset in main): nothing to commit; re-queue. Leaving the staged
      # merge in place parked the runner on "a merge is already in progress" for 95 minutes.
      log "MERGE STATE LOST for $branch: MERGE_HEAD=${staged_head:-<none>} branch=${branch_tip:-<none>} - aborting the stale merge and re-queueing the branch"
      git merge --abort >>"$LOG" 2>&1 || true
      echo "$branch" >> "$QUEUE"
      echo "$(date '+%m-%d %H:%M')  $branch  $ticket  MERGE_STATE_LOST (branch moved mid-gate; stale merge aborted, branch re-queued)" >> "$NEEDS"
      return 0
    fi
    # T-764: the commit can now be REFUSED — `.githooks/pre-commit` validates docs/tasks.yaml
    # before any commit that touches it, and a merge commit is one of the two writers that can
    # put a malformed board on main. An unchecked `git commit` here would log "MERGED ✓" for a
    # merge that never happened, which is the same false-success shape as T-840's lost MERGE_HEAD.
    if ! git commit -m "Merge $ticket ($branch): gate passed (automated merge, no AI)" >>"$LOG" 2>&1; then
      log "COMMIT REFUSED for $branch (pre-commit hook or hook failure) - NOT merged"
      git merge --abort 2>/dev/null || true
      echo "$(date '+%m-%d %H:%M')  $branch  $ticket  COMMIT_REFUSED" >> "$NEEDS"
      notify_coordinator "$ticket ($branch) gated GREEN but its merge COMMIT was refused (see the log; usually a malformed docs/tasks.yaml). main is untouched."
      return 0
    fi
    log "MERGED $branch ✓"
    record_landed "$branch"
    clear_attempts "$branch"
    echo "$(date '+%m-%d %H:%M')  $branch  $ticket  MERGED" >> "$DONELOG"
    local wt; wt=$(worktree_of "$branch")
    if [ -n "$wt" ] && [ "$(cd "$wt" && pwd -P)" != "$(cd "$REPO" && pwd -P)" ]; then
      git worktree remove "$wt" --force 2>>"$LOG" && log "worktree removed: $wt"
    fi
    notify_ok "MERGED $ticket ($branch); queue now $(grep -vcE '^[[:space:]]*(#|$)' "$QUEUE" 2>/dev/null || echo 0) waiting."
  else
    git merge --abort 2>/dev/null || true
    record_attempt "$branch" "$tip"
    log "GATE FAILED $branch (attempt $((tries+1))/$MAX_ATTEMPTS, tip $tip) -> abort + flag for AI"
    echo "$(date '+%m-%d %H:%M')  $branch  $ticket  GATE_FAIL" >> "$NEEDS"; notify_coordinator "$ticket ($branch) FAILED the merge gate (tests)."
  fi
  return 0
}

# THE GATE RUNS ALONE (user, 2026-09-22): no gate starts while a worker is running. The work
# runner stops dispatching once WORK_QUEUE_PAUSE branches wait here (and while a gate runs), so
# the running workers finish and the box empties; this is the other half. The SDET review of
# 2026-09-22 measured why: during shared gates, untouched crates of small unit tests ran 18-79x
# dearer (hk-recipe 79x) - contention, not code. Workers are counted from the work runner's
# claims (state=running), not from ps, so a wrapper process or a reviewer is not mistaken for
# one. After WORKER_DRAIN_MAX seconds of waiting the gate runs anyway and says so: a stuck
# worker must not hold every merge (the work runner releases stale claims after 4 h).
# RETIRED AS THE DEFAULT (user, 2026-09-23 13:30): WORKER_DRAIN_MAX=0 is OVERLAP mode - claimed
# workers no longer hold a gate at all; the work runner caps dispatch at the gate's reserve while
# one runs (ops/work-runner.py, WORK_GATE_ALONE). Measured 2026-09-23: with the gate alone the box
# alternated 45-min gates and 45-min drains, dispatch was zero in 10 of 13 hours, and landings
# fell to ~1/hour once the crisis backlog drained. The flake causes the rule was bought for are
# fixed at the root. What STILL holds a gate in overlap mode: a foreign spec run or `hk serve`
# (they share the gate's lane ports) and watchdog contention (unowned busy processes), both for
# at most FOREIGN_DRAIN_MAX. Set WORKER_DRAIN_MAX=2700 (with WORK_GATE_ALONE=1) for the old cycle.
WORKER_DRAIN_MAX=${WORKER_DRAIN_MAX:-0}
DRAIN_SINCE=""
workers_running(){
  local claimed foreign
  claimed=$(python3 - "$S/work-claims.json" <<'PY' 2>/dev/null || echo 0
import json, sys
try:
    d = json.load(open(sys.argv[1]))
    print(sum(1 for c in d.values() if c.get("state") == "running"))
except Exception:
    print(0)
PY
)
  # Browser-spec runs and test servers that are NOT this runner's (a triage or fix agent
  # reproducing a spec) share the ports and the CPU the gate's own browser tier needs; on
  # 2026-09-22 they turned three green specs red in two different gates. Count them as
  # workers: the gate waits for them the same way (and the same 45-min cap applies). This is
  # only consulted BEFORE a gate starts, when none of these can be the runner's own.
  foreign=$(pgrep -f 'node e2e/run.mjs|hk serve --bind 127.0.0.1:87' 2>/dev/null | wc -l | tr -d ' ')
  FOREIGN_RUNNING=${foreign:-0}   # read by workers_drained: a spec run is minutes, a worker is an hour
  # A claimed worker that is itself waiting for the gate (`just wait-for-gate`, a live pid under
  # $S/gate-waiters/) is idle, not contending: don't wait for it. On 2026-09-23 T-846 polled for
  # "no gate" while this runner waited for T-846, for the whole 2700 s drain cap. A marker whose
  # pid is gone is a waiter that was killed mid-wait; drop it.
  local waiting=0 m
  for m in "$S"/gate-waiters/*; do
    [ -e "$m" ] || continue
    if kill -0 "${m##*/}" 2>/dev/null; then waiting=$((waiting + 1)); else rm -f "$m"; fi
  done
  GATE_WAITERS=$waiting
  [ "$waiting" -gt 0 ] && [ "$claimed" -gt 0 ] && claimed=$(( claimed > waiting ? claimed - waiting : 0 ))
  echo $(( claimed + ${foreign:-0} ))
}
GATE_WAITERS=0
FOREIGN_RUNNING=0
# A foreign spec run holds the gate for at most this long. A deflaker that re-runs a spec every
# time it sees no gate, beside a runner that waits for the spec to end before starting one, is a
# standoff the 45-min worker cap resolves too slowly (02:27 on 2026-09-23: one 54-min worker and
# one spec run held a 12-branch batch).
FOREIGN_DRAIN_MAX=${FOREIGN_DRAIN_MAX:-300}
# While this waits it holds `$S/gate-wanted`, which the work runner reads as "a gate is
# pending: dispatch nothing" - otherwise, below WORK_QUEUE_PAUSE, dispatch would keep refilling
# the box and the drain would never complete (observed 14:12: T-565 started during the wait).
GATEWANT=$S/gate-wanted
# The claims file only knows about processes THIS orchestration started. On 2026-09-22 the box
# also carried sixteen orphaned 100 % busy shells belonging to an agent that had already exited,
# and every gate in two and a quarter hours ran beside them with `workers_running` reporting
# zero. `ops/watchdog.py` is what sees those; this reads its last tick. A stale tick (>3 min) is
# treated as "nothing known", never as "clear" - a dead watchdog must not silently license a
# contended gate, but it must not block every merge either.
# The rule itself lives in `ops/watchdog.py --contended` (unit-tested in py/tests/test_watchdog.py)
# rather than in a here-doc here, so it can be exercised without a merge runner and a loaded box.
contention(){ # echoes what the box is doing that this gate should not share; empty = clear
  python3 "$(dirname "${BASH_SOURCE[0]}")/watchdog.py" --contended 2>/dev/null
}
HK_GATE_CONTENDED=""; export HK_GATE_CONTENDED   # py/hkpy/gate.py prints and records it
workers_drained(){ # 0 = no worker running and the box is clear (or waited long enough), 1 = wait
  local n c why; n=$(workers_running); c=$(contention)
  if [ "${n:-0}" -eq 0 ] && [ -z "$c" ]; then
    DRAIN_SINCE=""; rm -f "$GATEWANT"; HK_GATE_CONTENDED=""; return 0
  fi
  # OVERLAP MODE (WORKER_DRAIN_MAX=0): claimed workers share the box with the gate. Only a
  # foreign spec run / hk serve (the gate's own lane ports) or watchdog contention still waits.
  if [ "$WORKER_DRAIN_MAX" -eq 0 ] && [ "${FOREIGN_RUNNING:-0}" -eq 0 ] && [ -z "$c" ]; then
    HK_GATE_CONTENDED="$n worker(s) running (overlap mode)"
    log "OVERLAP: $n worker(s) running - gating beside them; the work runner caps dispatch at the gate's reserve"
    DRAIN_SINCE=""; rm -f "$GATEWANT"; return 0
  fi
  why=""
  [ "${n:-0}" -gt 0 ] && why="$n worker(s) running"
  [ -n "$c" ] && why="${why:+$why; }contention: $c"
  [ -z "$DRAIN_SINCE" ] && { DRAIN_SINCE=$(date +%s); if [ "$WORKER_DRAIN_MAX" -eq 0 ]; then log "WAIT: $why - a foreign spec run or contention holds the gate (at most ${FOREIGN_DRAIN_MAX}s)"; else log "WAIT: $why - the gate runs alone, dispatch is paused"; fi; }
  printf 'since=%s\nworkers=%s\ncontention=%s\n' "$DRAIN_SINCE" "$n" "$c" > "$GATEWANT"
  # Only foreign spec runs / contention left (no claimed worker), or overlap mode: the short cap.
  local cap="$WORKER_DRAIN_MAX"
  if [ "$WORKER_DRAIN_MAX" -eq 0 ] || [ $(( ${n:-0} - ${FOREIGN_RUNNING:-0} )) -le 0 ]; then cap="$FOREIGN_DRAIN_MAX"; fi
  if [ $(( $(date +%s) - DRAIN_SINCE )) -ge "$cap" ]; then
    log "WAIT over: $why still, after $cap s - gating anyway (nothing stuck must hold every merge)"
    # The gate runs, but it is not a clean measurement of the code, and the gate log is the only
    # place that can still say so once the run is over.
    HK_GATE_CONTENDED="$why"
    [ -n "$c" ] && alert amber "gating a contended box" "Waited ${WORKER_DRAIN_MAX}s and gave up: $why. Timings from this gate are not comparable (see ops/watchdog.py)." --key "contended-gate"
    DRAIN_SINCE=""; rm -f "$GATEWANT"; return 0
  fi
  return 1
}
rm -f "$GATEWANT"   # a marker from a previous run must not outlive it

# preconditions for touching main; 0 = OK to proceed, 1 = wait
main_ready(){
  cd "$REPO" || return 1
  [ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || { log "WAIT: HEAD not on main"; return 1; }
  [ -e "$REPO/.git/MERGE_HEAD" ] && { log "WAIT: a merge is already in progress"; return 1; }
  git diff --quiet && git diff --cached --quiet || { log "WAIT: main tree dirty (coordinator mid-commit)"; return 1; }
  return 0
}

# print, one per line, the args that exist as branches AND are ahead of main
ready_filter(){
  local b a
  for b in "$@"; do
    git -C "$REPO" rev-parse --verify "$b" >/dev/null 2>&1 || { log "SKIP $b: no such branch"; continue; }
    a=$(git -C "$REPO" rev-list --count "main..$b" 2>/dev/null || echo 0)
    [ "${a:-0}" -eq 0 ] && { log "SKIP $b: nothing ahead of main (already merged?)"; continue; }
    printf '%s\n' "$b"
  done
}

# BULK: merge every queued branch, gate ONCE over the result, keep it only if green.
# 0 = all merged; 1 = caller should fall back to merging each individually.
#
# T-543 MEASURED WHY THIS MATTERS, AND WHY IT NEVER WORKED. `just cycle-time` over this
# very log: gate median 21.4 min, but QUEUE WAIT (a branch's last commit -> its gate)
# median 119.7 min, p90 452 min, and commit->merge median 272.6 min. The gate is about 8 %
# of a ticket's cycle; the queue behind N SERIAL gates is most of the rest. Bulk is
# therefore the single biggest lever available, and it had never once fired:
#
#   8 BULK attempts in the log, 0 merges. Every one "BULK conflict across branches",
#   and the attempt of 09-20 09:43 left a `git merge-octopus` wedged for over ten hours.
#
# The cause is `git merge A B C…` with three or more heads, which selects the OCTOPUS
# strategy. Octopus refuses outright any path that more than one head modified — it does
# not attempt a content merge at all. Nearly every branch here touches `docs/tasks.yaml`,
# so octopus was guaranteed to refuse, every time, and the "fall back to individual" line
# was not a rare safety net but the only path this code ever took.
#
# So: merge the branches ONE AT A TIME (two heads each, ordinary recursive merge, which
# does resolve a shared `tasks.yaml`), then run ONE gate over the accumulated result. The
# gate subject is `--base <pre-batch sha>`, i.e. exactly the commits this batch added and
# nothing else. On red the whole batch is rewound to that sha and the caller isolates by
# gating branches individually, which is the behaviour that was intended all along.
#
# Rewinding main with `reset --hard` is safe here and only here: this runner is the sole
# merger to main, nothing has been pushed, and the batch is reconstructible from the
# branches it merged. It is still guarded — the rewind happens only if HEAD is still the
# commit this function created, so a concurrent commit is never discarded.
FLAKY=$S/flaky.jsonl
# A HARD TIME LIMIT ON EVERY GATE (user, 2026-09-22: "a time-limit kill after 60 minutes").
# A gate that runs past GATE_TIMEOUT seconds is killed - its whole process group, so nextest,
# cargo, hk serve and Chrome go with it - and counts as a failure of kind "timeout", which is
# re-queued once (like a suite-wide red) rather than isolated. Job control (`set -m`) gives the
# background job its own process group, which is what makes the kill complete.
GATE_TIMEOUT=${GATE_TIMEOUT:-3600}
GATE_TIMED_OUT=0
limited(){ # limited <cmd...>  -> the command's exit code, or 124 on timeout
  local pid start now
  GATE_TIMED_OUT=0
  set -m
  ( cd "$REPO" && "$@" ) >>"$LOG" 2>&1 &
  pid=$!
  set +m
  start=$(date +%s)
  while kill -0 "$pid" 2>/dev/null; do
    now=$(date +%s)
    if [ $(( now - start )) -ge "$GATE_TIMEOUT" ]; then
      log "GATE TIMEOUT: '$*' exceeded ${GATE_TIMEOUT}s - killing its process group"
      alert red "gate killed at ${GATE_TIMEOUT}s" "$* - process group killed; the batch is re-queued once. Load: $(uptime | sed 's/.*load averages*: *//')" --key "timeout:$*"
      kill -TERM -- "-$pid" 2>/dev/null; sleep 20; kill -KILL -- "-$pid" 2>/dev/null
      GATE_TIMED_OUT=1
      wait "$pid" 2>/dev/null
      return 124
    fi
    sleep 10
  done
  wait "$pid"; return $?
}

# THE LEDGER (py/hkpy/flakes.py). Every triage above is a forgiveness: a test that passes alone
# costs a gate, gets retried, and is forgotten - so the same spec can cost four gates in one day
# and nothing anywhere counts to two. This reads BOTH of the runner's own outputs (flaky.jsonl
# for the load, the TRIAGE lines for the failed-alone direction the jsonl never records), counts
# per test, and at two reds in seven days writes ONE line into merge-needs-attention.txt with
# the evidence attached, so the coordinator files it from data instead of rediscovering it.
#
# Best effort, always: it runs AFTER the triage decision has already been made and returned, it
# cannot change that decision, and `|| true` plus a total `try/except` inside mean a broken
# ledger can never fail a merge. A measurement must not be able to break what it measures.
flake_ledger(){
  ( cd "$REPO" && uv run --locked --project py python -m hkpy.flakes --update ) >>"$LOG" 2>&1 || true
}

# One wrapper, one call: every path out of the triage (passed alone, failed alone, not a flake
# candidate) updates the ledger, without four copies of the same line inside the branches.
flake_retry(){
  _flake_retry "$@"; local rc=$?
  flake_ledger
  return $rc
}

_flake_retry(){ # base gate_log_start_line tickets [retry_cmd] -> exit 0 if the retried gate passed
  # retry_cmd defaults to `just gate --base $base` (a bulk, already committed on main); the
  # single-branch path passes `just gate-merge`, because its merge is still STAGED and a
  # `--base` gate would diff the wrong thing.
  local base=$1 from=$2 tickets=$3 retry=${4:-"just gate --base $base"} tests filter t rc
  # nextest prints `FAIL [` for a plain failure and `TRY n FAIL [` once .config/nextest.toml
  # gives a test retries (T-841); a test that passed on a retry prints `FLAKY` and is not red.
  tests=$(tail -n +"$from" "$LOG" | grep -E '^\s+(TRY [0-9]+ )?FAIL \[' | awk '{print $NF}' | sort -u)
  TRIAGE_KIND="test"
  # The browser tier (ui/e2e/run.mjs) reports its reds on one summary line, not as nextest FAIL
  # lines: `e2e: 11/13 files passed in 662.5 s (backend 2.9 s); failed: fog-of-war.e2e.mjs, ...`.
  # Those are TEST failures too (2026-09-22 14:49 they were read as "suite broken"), and a spec
  # can be re-run alone with `npm run e2e -- <name>...` from ui/.
  local specs; specs=$(tail -n +"$from" "$LOG" | grep -E '^e2e: [0-9]+/[0-9]+ files passed .*; failed: ' | tail -1 | sed 's/.*failed: //' | tr -d ',')
  if [ -z "$tests" ] && [ -n "$specs" ]; then
    TRIAGE_SPECS="$specs"   # try_bulk re-runs these on main alone if this batch is red
    log "TRIAGE: browser specs red: $specs - re-running them alone"
    if ( cd "$REPO/ui" && npm run e2e -- $specs ) >>"$LOG" 2>&1; then
      # The browser tier is the LAST suite; the Rust workspace and acceptance already passed
      # on this exact tree, so the retry re-runs only the acceptance phase (acceptance-ci +
      # test-ui-e2e), not the 15-minute workspace suite again.
      log "TRIAGE: they PASS alone -> load flake; retrying the gate's acceptance phase once"
      printf '{"ts":"%s","tests":"%s","batch":"%s","load_before":"%s"}\n' "$(date '+%Y-%m-%dT%H:%M:%S')" "$specs" "$tickets" "$(uptime | sed 's/.*load averages*: *//')" >> "$S/flaky.jsonl"
      limited $retry --phase acceptance; rc=$?
      [ "$rc" -eq 0 ] && log "TRIAGE: retry PASSED" || log "TRIAGE: retry FAILED too -> not a flake we can wait out"
      return $rc
    fi
    log "TRIAGE: a browser spec FAILS alone -> a real defect in this merge"
    return 1
  fi
  [ -z "$tests" ] && { TRIAGE_KIND="suite"; log "TRIAGE: no FAIL lines found (lint/build/ui-unit failure) - not a flake candidate"; return 1; }
  filter=""; for t in $tests; do filter="${filter:+$filter | }test(${t##*::})"; done
  TRIAGE_FILTER="$filter"   # try_bulk re-runs the same set on main alone if this batch is red
  log "TRIAGE: re-running the failing tests alone: $(echo $tests | tr '\n' ' ')"
  # Workers are bounded (ops/work-runner.py: build jobs, test threads, background QoS) and the gate
  # has its reserved cores, so nothing here asks anyone to step aside: the re-run and the retry get
  # the reserve the gate always has.
  if ( cd "$REPO" && cargo nextest run --workspace -E "$filter" ) >>"$LOG" 2>&1; then
    log "TRIAGE: they PASS alone -> load flake; retrying the full gate once"
    printf '{"ts":"%s","tests":"%s","batch":"%s","load_before":"%s"}\n' "$(date '+%Y-%m-%dT%H:%M:%S')" "$(echo $tests | tr '\n' ' ')" "$tickets" "$(uptime | sed 's/.*load averages*: *//')" >> "$FLAKY"
    limited $retry; rc=$?
    [ "$rc" -eq 0 ] && log "TRIAGE: retry PASSED" || log "TRIAGE: retry FAILED too -> not a flake we can wait out"
    return $rc
  fi
  log "TRIAGE: a test FAILS alone -> a real defect in this merge"
  return 1
}

try_bulk(){
  local branches=("$@") tickets="" b wt base after rc
  for b in "${branches[@]}"; do tickets="$tickets $(ticket_of "$b")"; done
  tickets="${tickets# }"
  log "BULK attempt (${#branches[@]}): ${branches[*]}"
  if [ "$DRY_RUN" = "1" ]; then log "DRY-RUN would bulk-merge: $tickets"; return 0; fi
  base=$(git -C "$REPO" rev-parse HEAD)
  # Declare the provisional window BEFORE the first merge commit, so there is no instant in which
  # main carries an ungated commit and nothing says so.
  { echo "base=$base"; echo "started=$(date '+%Y-%m-%d %H:%M:%S')"; echo "pid=$$";
    echo "branches=${branches[*]}"; } > "$BULKMARK"
  # A branch that will not merge is the branch to SET ASIDE, not a reason to un-merge the ones
  # that did. Rewinding the whole batch on the first conflict is what happened on 2026-09-21:
  # `task-t559` conflicted on the justfile (two recipes added at the same line) and the other
  # FOURTEEN branches - already merged cleanly in this very loop - were reset and sent through
  # individual gates instead, turning one trivial conflict into ~5 full gates of code branches
  # at ~21 min each. T-543 measured the gate at ~8 % of a ticket cycle and the QUEUE behind
  # serial gates as most of the rest, so this fallback spent the exact resource the bulk path
  # exists to save.
  #
  # So: skip the conflicting branch, keep the batch, and flag the skipped one for a person the
  # same way an individual CONFLICT is flagged. It is still flagged and never silently dropped,
  # and it is NOT re-queued here - a conflict needs a fix, not a retry (the unchanged-since-fail
  # rule).
  local merged=() skipped=""
  for b in "${branches[@]}"; do
    if git -C "$REPO" merge --no-ff -m "Merge $(ticket_of "$b") ($b): batch, gated together (automated, no AI)" "$b" >>"$LOG" 2>&1; then
      merged+=("$b")
    else
      git -C "$REPO" merge --abort 2>/dev/null || true
      skipped="$skipped $b"
      log "BULK conflict merging $b -> SKIPPED, batch continues with the rest"
      echo "$(date '+%m-%d %H:%M')  $b  $(ticket_of "$b")  CONFLICT(skipped from bulk)" >> "$NEEDS"
    fi
  done
  [ -n "$skipped" ] && log "BULK skipped (need a fix, not a retry):$skipped"
  if [ "${#merged[@]}" -eq 0 ]; then
    log "BULK every branch conflicted -> nothing to gate"
    git -C "$REPO" reset --hard "$base" >>"$LOG" 2>&1
    rm -f "$BULKMARK"
    return 0
  fi
  # Re-point the batch at what actually merged, so the gate, the done-log, the landed ledger and
  # the worktree removals below all speak about the same set.
  branches=("${merged[@]}")
  BULK_MERGED_LIST="${merged[*]}"
  tickets=""
  for b in "${branches[@]}"; do tickets="$tickets $(ticket_of "$b")"; done
  tickets="${tickets# }"
  after=$(git -C "$REPO" rev-parse HEAD)
  echo "after=$after" >> "$BULKMARK"
  log "BULK gate (just gate --base $base over ${#branches[@]} merged branches; may take 15-25 min)…"
  # $(( )) strips the leading spaces macOS `wc -l` prints; `tail -n +"   381417"` is an
  # "illegal offset", prints nothing, and flake_retry then saw "no FAIL lines" on every red
  # gate it was ever given (2026-09-22 13:55: one flake -> 14 branches isolated).
  local gate_line; gate_line=$(( $(wc -l < "$LOG") ))
  limited just gate --base "$base"; rc=$?
  # TRIAGE BEFORE ISOLATING. A red batch used to mean "rewind and re-gate every branch alone" -
  # 22 branches x 50 min on 2026-09-22, for one load-sensitive test no branch had touched. Now the
  # failing tests are re-run ALONE first (seconds to minutes); if they pass alone it is a load flake,
  # recorded in flaky.jsonl, and the whole gate is retried ONCE (workers are bounded, so the
  # gate's reserved cores are the gate's - nothing is suspended). Only a test that fails alone,
  # or a second red gate, still isolates.
  if [ "$rc" -ne 0 ] && [ "$GATE_TIMED_OUT" = 1 ]; then
    # A timed-out gate proves nothing about any test: treat it as a suite-wide red - rewind,
    # re-queue the batch once, hold until the queue changes - and say so where a person looks.
    TRIAGE_KIND="suite"; TRIAGE_FILTER=""; TRIAGE_SPECS=""
    echo "$(date '+%m-%d %H:%M')  (bulk)  $tickets  GATE_TIMEOUT after ${GATE_TIMEOUT}s - killed; batch re-queued once" >> "$NEEDS"
  elif [ "$rc" -ne 0 ]; then flake_retry "$base" "$gate_line" "$tickets"; rc=$?; fi
  if [ "$rc" -eq 0 ]; then
    log "BULK MERGED ✓ $tickets"
    for b in "${branches[@]}"; do
      echo "$(date '+%m-%d %H:%M')  $b  $(ticket_of "$b")  MERGED(bulk)" >> "$DONELOG"
      record_landed "$b"
      clear_attempts "$b"
      wt=$(worktree_of "$b")
      [ -n "$wt" ] && [ "$wt" != "$REPO" ] && git -C "$REPO" worktree remove "$wt" --force 2>>"$LOG" && log "worktree removed: $wt"
    done
    rm -f "$BULKMARK"
    notify_ok "MERGED batch ($tickets); queue now $(grep -vcE '^[[:space:]]*(#|$)' "$QUEUE" 2>/dev/null || echo 0) waiting."
    return 0
  fi
  if [ "$(git -C "$REPO" rev-parse HEAD)" = "$after" ]; then
    git -C "$REPO" reset --hard "$base" >>"$LOG" 2>&1
    # A red with NO test FAIL line is lint, a build error or the UI unit step - a property of
    # main+batch as a whole that every isolated gate would reproduce (2026-09-22 14:34: a
    # TypeScript type error on main itself; isolating 6 branches would have been 6 identical
    # reds, 6 attempt-ledger strikes and ~90 min). So: rewind, put the batch BACK in the queue
    # in order, flag it once, and wait for a fix to be queued - never isolate.
    if [ "${TRIAGE_KIND:-test}" = "suite" ]; then
      for b in "${branches[@]}"; do echo "$b" >> "$QUEUE"; done
      batch_sig "${branches[@]}" > "$S/suite-broken"
      log "BULK gate FAILED without a test FAIL (lint/build/ui-unit) -> rewound to $base; batch re-queued in order, NOT isolated - main+batch needs a fix"
      echo "$(date '+%m-%d %H:%M')  (bulk)  $tickets  SUITE_BROKEN - no test FAIL; lint/build/ui-unit red on main+batch; fix and queue the fix, the batch is re-queued behind it" >> "$NEEDS"
      notify_coordinator "batch ($tickets) failed WITHOUT a test failure - lint/build/ui-unit is red on main+batch; fix that first, the batch is re-queued."
      rm -f "$BULKMARK"
      return 0
    fi
    # IS MAIN ITSELF RED? A test that fails alone on main+batch and ALSO fails alone on the
    # rewound main is main's defect, and isolating would only re-prove it once per branch
    # (15:45 on 2026-09-22: three branches isolated against a lattice test that main had
    # failed since a hand-landed batch; the fix branch was sitting in the queue). Costs one
    # scoped nextest run on the clean main; saves a full gate per branch.
    if [ "${TRIAGE_KIND:-test}" = "test" ] && [ -n "${TRIAGE_FILTER:-}" ]; then
      log "TRIAGE: is main itself red? re-running the failing tests alone on the rewound main"
      if ! ( cd "$REPO" && cargo nextest run --workspace -E "$TRIAGE_FILTER" ) >>"$LOG" 2>&1; then
        for b in "${branches[@]}"; do echo "$b" >> "$QUEUE"; done
        batch_sig "${branches[@]}" > "$S/suite-broken"
        log "TRIAGE: MAIN IS RED on: $(echo $TRIAGE_FILTER) -> batch re-queued in order, NOT isolated; queue the fix"
        echo "$(date '+%m-%d %H:%M')  (bulk)  $tickets  MAIN_RED - the failing test(s) fail on main itself ($TRIAGE_FILTER); fix main, the batch is re-queued behind the fix" >> "$NEEDS"
        notify_coordinator "main itself fails $TRIAGE_FILTER - the batch ($tickets) is re-queued and held; queue a fix for main."
        rm -f "$BULKMARK"
        return 0
      fi
      log "TRIAGE: main is green on them -> the batch introduced it; isolating"
    fi
    # The same question for browser specs (fog-of-war on 2026-09-22 16:22: red on main since
    # T-580 landed in the hand fast-forward, and the batch would have been isolated four times).
    if [ "${TRIAGE_KIND:-test}" = "test" ] && [ -z "${TRIAGE_FILTER:-}" ] && [ -n "${TRIAGE_SPECS:-}" ]; then
      log "TRIAGE: is main itself red? re-running the browser specs alone on the rewound main: $TRIAGE_SPECS"
      # REBUILD FIRST. The spec runner serves whatever `target/debug/hk` and `ui/dist` already exist
      # (ui/e2e/backend.mjs), and those were built from the BATCH tree by the gate that just failed.
      # At 13:47 on 2026-09-23 this step ran main's spec against the batch's UI bundle - which
      # carried the very tilecache.ts change surface-nav was red on - and declared MAIN IS RED,
      # re-queueing eight branches behind a defect that belonged to one of them. `just test-ui-e2e`
      # rebuilds both before it runs; this path must too, or its verdict is about the wrong tree.
      log "TRIAGE: rebuilding hk and ui/dist from the rewound main before the spec re-run"
      ( cd "$REPO" && cargo build -q -p hk-cli --bin hk && cd ui && npm run build ) >>"$LOG" 2>&1 \
        || log "TRIAGE: WARN rebuild failed; the spec re-run below may test the batch's artefacts"
      if ! ( cd "$REPO/ui" && npm run e2e -- $TRIAGE_SPECS ) >>"$LOG" 2>&1; then
        for b in "${branches[@]}"; do echo "$b" >> "$QUEUE"; done
        batch_sig "${branches[@]}" > "$S/suite-broken"
        log "TRIAGE: MAIN IS RED on browser spec(s): $TRIAGE_SPECS -> batch re-queued in order, NOT isolated; queue the fix"
        echo "$(date '+%m-%d %H:%M')  (bulk)  $tickets  MAIN_RED - browser spec(s) $TRIAGE_SPECS fail on main itself; fix main, the batch is re-queued behind the fix" >> "$NEEDS"
        notify_coordinator "main itself fails browser spec(s) $TRIAGE_SPECS - the batch ($tickets) is re-queued and held; queue a fix for main."
        rm -f "$BULKMARK"
        return 0
      fi
      log "TRIAGE: main is green on them -> the batch introduced it; isolating"
    fi
    log "BULK gate FAILED -> rewound to $base; isolate by merging each individually"
  else
    log "BULK gate FAILED but HEAD moved since the batch - NOT rewinding; needs a person"
    echo "$(date '+%m-%d %H:%M')  (bulk)  $tickets  BULK_FAIL_HEAD_MOVED" >> "$NEEDS"
  fi
  # A rewind restores main to a gated commit, so the window is over. The HEAD-MOVED branch does NOT
  # rewind and main is left carrying ungated commits, so the marker STAYS - that is precisely the
  # case a person has to be told about, and the stale marker is the telling.
  if [ "$(git -C "$REPO" rev-parse HEAD)" = "$base" ]; then rm -f "$BULKMARK"; fi
  return 1
}

SEEN_QUEUED=""   # branches already logged as QUEUED this run (T-543)

# Say WHICH COPY of this script is running, and whether it matches the repo.
#
# On 2026-09-20 the runner had been started from a stale copy in $HACKRIFF_OPS rather than
# from ops/merge-runner.sh. The sequential-bulk fix was committed, reviewed and believed
# live for hours while the process kept executing the old octopus code from its own inode -
# eight bulk attempts, zero bulk merges, and a log that gave no hint the running code was
# not the committed code. A restart is the only way to pick up an edit, so the log must at
# least say what it is running: a stale runner is invisible otherwise.
#
# This only REPORTS. It never re-execs itself - swapping code under a live gate is worse
# than running old code, and the decision to restart belongs to whoever is watching.
self_version(){
  local self repo_copy
  self=${BASH_SOURCE[0]}
  repo_copy=$(git -C "$REPO" show HEAD:ops/merge-runner.sh 2>/dev/null)
  if [ -z "$repo_copy" ]; then log "VERSION: $self (no repo copy to compare against)"; return; fi
  if [ "$(cat "$self" 2>/dev/null)" = "$repo_copy" ]; then
    log "VERSION: $self matches $(git -C "$REPO" rev-parse --short HEAD):ops/merge-runner.sh"
  else
    log "VERSION: *** STALE *** $self DIFFERS from $(git -C "$REPO" rev-parse --short HEAD):ops/merge-runner.sh"
    log "VERSION: restart from the repo between gates - see ops/README.md - or this runner keeps executing old code"
  fi
}

# The board's merge driver is named by .gitattributes (committed) but implemented by a command
# in .git/config (not committed), so a fresh clone fails every tasks.yaml merge with "custom
# merge driver hkboard lacks command line". Register it here too: the runner is the one path
# that must never be tripped by a setup step nobody ran.
( cd "$REPO" && just setup-git ) >>"$LOG" 2>&1 || log "WARN: just setup-git failed; tasks.yaml merges may conflict"

log "=== merge-runner up (DRY_RUN=$DRY_RUN, bulk mode); watching $QUEUE ==="
# What this process is actually running with - `just knobs show` reads it back as "effective".
log "KNOBS: WORKER_DRAIN_MAX=$WORKER_DRAIN_MAX FOREIGN_DRAIN_MAX=$FOREIGN_DRAIN_MAX BULK_MAX=$BULK_MAX GATE_TIMEOUT=$GATE_TIMEOUT MAX_ATTEMPTS=$MAX_ATTEMPTS"
self_version
# STARTUP REPAIR (user, 2026-09-22 16:55: "Why would I need to abort a merge? Shouldn't that
# happen automatically?"). This runner is the only writer of main, so a staged merge or a
# bulk marker found at startup can only be a previous runner's, killed mid-gate. Nothing was
# gated, nothing was committed: abort the staged merge, rewind a provisional bulk to its base,
# re-queue those branches, and carry on. Waiting for a person to type `git merge --abort`
# cost an hour of an empty box today. Refuse only if the tree has uncommitted edits that are
# not the merge's own - that is someone else's work and a person must look.
# ORPHAN GATES FIRST. A runner killed mid-gate leaves its gate subshell (just gate / cargo /
# nextest / npm e2e / hk serve) running on this very checkout; at 17:58 on 2026-09-22 a new
# runner started a second gate beside one such orphan and the two shared the tree, the ports
# and the CPU for 30 minutes (run ids d1503dd80480 and 4ecba9c10c0a). At startup NO gate
# process can be legitimate, so every one of them is killed before anything else.
# ONLY ON THIS CHECKOUT, though. The gate runs in $REPO; a worker's targeted `cargo nextest run`
# in its own worktree matches the same pattern and is not ours to kill - at 05:13 on 2026-09-23
# a restart killed a coordinator's `nextest run -p hk-cli -E binary(api_contract)` this way.
# The process's cwd decides: under $REPO but not under $REPO/.claude/worktrees/ is the gate's
# tree; anywhere else is someone else's run and is left alone (and said so).
for pat in '^just gate' 'python -m hkpy.gate' 'cargo-nextest nextest run' 'node e2e/run.mjs' 'npm run e2e' 'hk serve --bind 127.0.0.1:87'; do
  for opid in $(pgrep -f "$pat" 2>/dev/null); do
    [ "$opid" = "$$" ] && continue
    ocwd=$(lsof -a -p "$opid" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -1)
    case "$ocwd" in
      "$REPO"/.claude/worktrees/*) log "STARTUP: leaving $opid ($pat) alone - it runs in a worktree ($ocwd)"; continue ;;
      "$REPO"|"$REPO"/*) ;;
      *) log "STARTUP: leaving $opid ($pat) alone - not on this checkout (cwd ${ocwd:-unknown})"; continue ;;
    esac
    log "STARTUP: killing orphan gate process $opid ($pat)"; kill -TERM "$opid" 2>/dev/null
  done
done
sleep 2
if [ -e "$REPO/.git/MERGE_HEAD" ]; then
  stale=$(git -C "$REPO" rev-parse --short MERGE_HEAD 2>/dev/null)
  git -C "$REPO" merge --abort >>"$LOG" 2>&1 && log "STARTUP: aborted a staged merge ($stale) a killed gate left behind" \
    || log "STARTUP: could not abort the staged merge ($stale) - a person must look"
fi
if [ -f "$BULKMARK" ]; then
  sbase=$(sed -n 's/^base=//p' "$BULKMARK"); sbranches=$(sed -n 's/^branches=//p' "$BULKMARK")
  if [ -n "$sbase" ] && git -C "$REPO" diff --quiet && git -C "$REPO" diff --cached --quiet; then
    git -C "$REPO" reset --hard "$sbase" >>"$LOG" 2>&1 && rm -f "$BULKMARK" \
      && log "STARTUP: rewound a provisional bulk to $sbase and re-queued: $sbranches" \
      && for b in $sbranches; do echo "$b" >> "$QUEUE"; done
  else
    log "STARTUP: bulk marker present but the tree is dirty or base unknown - NOT rewinding; a person must look"
  fi
fi
while true; do
  # read every queued (non-comment) branch, in order
  # NOTE: strip whitespace PER LINE — a plain `tr -d '[:space:]'` deletes the newlines
  # too and glues every queued branch into one unmergeable name (observed 2026-09-20).
  queued=$(grep -vE '^\s*(#|$)' "$QUEUE" 2>/dev/null | awk '{gsub(/[[:space:]]/,""); if ($0 != "") print}' || true)
  # T-543: log the moment a branch is first SEEN in the queue. `MERGE start` already says
  # when its gate began; the gap between the two is the queue wait, which `just cycle-time`
  # measured at a median of 119.7 min against a 21.4 min gate. Without this line that gap
  # has to be inferred from the branch's last commit, which also counts the time an agent
  # spent finishing up. One line, and only ever the first sighting per branch.
  for qb in $queued; do
    case " $SEEN_QUEUED " in *" $qb "*) ;; *) SEEN_QUEUED="$SEEN_QUEUED $qb"; log "QUEUED $qb";; esac
  done
  # A BOUNDED HOLD (pipeline manager, 2026-09-23; invariants 4-6 in .claude/rules/pipeline-
  # invariants.md). `$S/hold` is written by `just hold` with until=/why=/owner= and at most 30
  # minutes; this runner takes no branch while it is live, IGNORES it once expired, and ENDS it the
  # moment a branch is queued - a hold means "prefer idle", never "refuse work". Every end is
  # logged and the early end alerts, so a hold that cost anything is visible within a minute.
  if [ -f "$S/hold" ]; then
    hold_until=$(sed -n 's/^until=//p' "$S/hold" | head -1); hold_why=$(sed -n 's/^why=//p' "$S/hold" | head -1)
    if [ -z "$hold_until" ] || [ "$(date +%s)" -ge "${hold_until:-0}" ]; then
      rm -f "$S/hold"; log "HOLD expired ($hold_why) - resuming"
      printf '{"ts":%s,"event":"expired","why":%s}\n' "$(date +%s)" "\"${hold_why//\"/\\\"}\"" >> "$S/hold.jsonl"
    elif [ -n "$queued" ]; then
      rm -f "$S/hold"; log "HOLD ended at the first queued branch ($hold_why): $(echo $queued | cut -c1-80)"
      printf '{"ts":%s,"event":"ended-by-queue","why":%s}\n' "$(date +%s)" "\"${hold_why//\"/\\\"}\"" >> "$S/hold.jsonl"
      alert amber "hold ended by queued work" "$hold_why - a branch arrived; the runner resumed. Blocked minutes are charged to the open experiment." --key "hold-ended"
    else
      [ -z "${HOLD_SAID:-}" ] && { log "HOLD: merge queue held until $(date -r "$hold_until" '+%H:%M' 2>/dev/null || echo "$hold_until") - $hold_why"; HOLD_SAID=1; }
      sleep 8; continue
    fi
  fi
  HOLD_SAID=""
  if [ -n "$queued" ] && main_ready && workers_drained; then
    # keep only branches that still exist and are ahead of main
    ready=$(ready_filter $queued)
    # Dedupe, first occurrence wins: a branch appended more than once (each new tip re-queues the
    # same name) took one BULK_MAX slot per copy - `task-alerts` x4 pushed gate-diag, spec-waits
    # and watchdog out of the 03:12 batch on 2026-09-23.
    ready=$(printf '%s\n' $ready | awk '!seen[$0]++' | tr '\n' ' ')
    # drop the non-comment lines we're about to act on (keep comments); transient branches get requeued
    grep -E '^\s*#' "$QUEUE" > "$QUEUE.tmp" 2>/dev/null || true; mv "$QUEUE.tmp" "$QUEUE" 2>/dev/null || true
    # After a SUITE_BROKEN rewind the same batch would only fail the same way every ~15 min:
    # hold it until the queue changes (a fix branch appears, a branch is withdrawn, or a queued
    # branch's tip moves - a fix pushed to the branch itself releases the hold too).
    if [ -f "$S/suite-broken" ] && [ "$(batch_sig $ready)" = "$(cat "$S/suite-broken")" ]; then
      for b in $ready; do echo "$b" >> "$QUEUE"; done
      [ -z "${SUITE_HOLD_SAID:-}" ] && { log "HOLD: the same batch failed without a test FAIL; waiting for the queue to change (a fix)"; SUITE_HOLD_SAID=1; }
      sleep 8; continue
    fi
    rm -f "$S/suite-broken"; SUITE_HOLD_SAID=""
    set -- $ready
    # BATCH CAP (user, 2026-09-22: "reduce batch size to 15"). A 20-branch batch that goes red
    # is 20 branches' worth of isolation; the rest of the queue keeps its order and goes in the
    # next batch, so nothing is dropped - the tail is written back BEFORE anything runs.
    if [ "$#" -gt "$BULK_MAX" ]; then
      log "BULK cap: $# ready, taking the first $BULK_MAX; the other $(( $# - BULK_MAX )) stay queued in order"
      i=0; for b in "$@"; do i=$((i+1)); [ "$i" -gt "$BULK_MAX" ] && echo "$b" >> "$QUEUE"; done
      set -- "${@:1:$BULK_MAX}"
    fi
    if [ "$#" -eq 1 ]; then
      process "$1" || echo "$1" >> "$QUEUE"
    elif [ "$#" -ge 2 ]; then
      BULK_MERGED_LIST=""
      if ! try_bulk "$@"; then
        # Isolate only what the batch actually merged. A branch try_bulk SKIPPED conflicted, and
        # is already flagged in merge-needs-attention.txt; sending it round again just conflicts
        # a second time and writes a duplicate flag.
        isolate="${BULK_MERGED_LIST:-$*}"
        log "falling back to individual gates for: $isolate"
        for b in $isolate; do process "$b" || echo "$b" >> "$QUEUE"; done
      fi
    fi
  fi
  sleep 8
done
