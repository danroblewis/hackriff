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
# THE GATE BUILDS IN ITS OWN TARGET DIR (supervisor for the user, 2026-09-24 17:20, disk 303 -> 179 GB in
# 70 min): every gate rebuild of main's target/ in place turned the shared blocks of every worker worktree
# target - each a `cp -c` clone of main's - exclusive, ~2 GB/min. Every cargo this runner starts (gates,
# flake re-runs, main-red rebuilds) builds here instead, so main's target/ stays a stable clone source.
# Seeded as a clone of main's target/ at startup, so the first gate is warm. CI and hand gates unaffected.
export CARGO_TARGET_DIR="$S/gate-target"
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
  # `|| [ -n "$k" ]` keeps a last line without a newline; CR and surrounding spaces are stripped
  # so a hand-edited store reads the same here as in ops/work-runner.py (review, 2026-09-23).
  while IFS='=' read -r k v || [ -n "$k" ]; do
    k="${k//$'\r'/}"; k="${k#"${k%%[![:space:]]*}"}"; k="${k%"${k##*[![:space:]]}"}"
    v="${v//$'\r'/}"; v="${v#"${v%%[![:space:]]*}"}"; v="${v%"${v##*[![:space:]]}"}"
    case "$k" in ''|'#'*) continue ;; esac
    [[ "$k" =~ ^[A-Z][A-Z0-9_]*$ ]] || continue
    [ -z "${!k+x}" ] && export "$k=$v"
  done < "$S/env"
fi
MAX_ATTEMPTS=${MAX_ATTEMPTS:-2}
# Most branches one batch may carry (user, 2026-09-22); the rest keep their queue order.
BULK_MAX=${BULK_MAX:-15}
# GATE TIERS (user, 2026-09-24 20:00: "GATE_TIERS=check now"). `full` (the default) gates every merge
# with both phases; `check` gates it with `--phase check` only (lint + test for `full`-class diffs,
# test-ui for `ui`, the py/ops suites) - the split CI has: check per push, acceptance once a day.
# The acceptance phase's home under `check` is the daily release-candidate run (`just rc`, user rule
# the same evening: its reds become P1 tickets, never an un-land). Rollback: GATE_TIERS=full.
GATE_TIERS=${GATE_TIERS:-full}
# THE RELEASE CANDIDATE (user rule, 2026-09-24 20:00): under GATE_TIERS=check the acceptance phase leaves the
# merge gate and runs once a day - at the first gap between gates after RC_HOUR - and on `just rc`, over main's
# landed tip. Green tags rc-YYYYMMDD; each red is one P1 attention item. Never an un-land.
RC_HOUR=${RC_HOUR:-3}
RCMARK=$S/rc-in-progress
case "$GATE_TIERS" in full) GATE_PHASE="" ;; check) GATE_PHASE="--phase check" ;;
  *) echo "merge-runner: GATE_TIERS=$GATE_TIERS is not full|check - using full" >&2; GATE_TIERS=full; GATE_PHASE="" ;; esac
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
# Never from a worktree (ops/launch-guard.sh): logs PATH:, refuses before the startup repair or any merge.
. "$(dirname "${BASH_SOURCE[0]}")/launch-guard.sh"; launch_guard "${BASH_SOURCE[0]}"
# Discord (user, 2026-09-23): every exception the runner hands to a person is also an alert;
# every landing is a green one-liner. ops/alert.py dedupes by key and never fails the caller.
alert(){ python3 "$(dirname "${BASH_SOURCE[0]}")/alert.py" "$@" >/dev/null 2>&1 || true; }
# The identity of a batch for the suite-broken hold: branch NAMES AND TIPS, sorted. A hold keyed
# on names alone never released on a fix pushed to a queued branch - at 04:06 on 2026-09-23 the
# fixed branch sat behind "waiting for the queue to change" until a person deleted the marker.
batch_sig(){ for b in "$@"; do printf '%s@%s\n' "$b" "$(git -C "$REPO" rev-parse --short "$b" 2>/dev/null)"; done | sort | tr '\n' ' '; }
# One hold.jsonl line, JSON-encoded by Python so a `\`, a tab or a quote in `why` cannot produce a
# record `hkpy.flow` would silently drop (review, 2026-09-23). event: expired | ended-by-queue.
hold_event(){ python3 -c 'import json,sys,time; print(json.dumps({"ts": int(time.time()), "event": sys.argv[1], "why": sys.argv[2]}))' "$1" "$2" >> "$S/hold.jsonl" 2>/dev/null || true; }
# The coordinator's pane is gone (incident 2026-09-24 04:07: dead 5.5 h, four alarms typed at nothing).
no_receiver(){ python3 "$REPO/ops/alert.py" --no-receiver dev "MERGE-RUNNER: $1" >/dev/null 2>&1 || true; }
# $2 is the Discord title. "needs a person" is reserved for what no automation will pick up (user,
# 2026-09-24 11:40: six "needs a person" alerts that day were fix-run outcomes, and the user came
# asking what to decide); a red the work runner resumes a worker for says "fix run".
notify_coordinator(){
  alert amber "${2:-merge runner: coordinator action}" "$1" --key "mr:$(echo "$1" | cut -c1-48)"
  tmux has-session -t dev 2>/dev/null || { no_receiver "$1"; return 0; }; tmux send-keys -t dev -l "MERGE-RUNNER: $1 See $NEEDS; fix it, then re-queue the branch." 2>/dev/null; sleep 1; tmux send-keys -t dev Enter 2>/dev/null; }
# Edge-triggered wake on a SUCCESSFUL merge: a clean merge drains the queue and may unblock
# dependent tickets, but nothing else pings the coordinator for it (task-completions and the
# failure ping above cover their cases). Without this, the coordinator can sit idle after a
# green merge with startable work undone. It says "reconcile", never a computed to-do list:
# the coordinator's `just reconcile` is the single source of truth, and any list we pasted here
# would be stale by the time it acts.
notify_ok(){ # coordinator_notice [header branch...]
  # Discord gets release notes (user, 2026-09-23): the header, then one line per landed branch - the
  # ticket id AND its board title, or a non-ticket branch with its first commit subject
  # (py/hkpy/landnotes.py; under the 2000-char limit, "+N more"). The coordinator's pane keeps the
  # short notice.
  local notice=$1 header=${2:-$1} body=""; shift; [ "$#" -gt 0 ] && shift
  [ "$#" -gt 0 ] && body=$(cd "$REPO" && uv run --locked --project py python -m hkpy.landnotes --header "$header" "$@" 2>/dev/null)
  alert green "landed" "${body:-$notice}"
  tmux has-session -t dev 2>/dev/null || { no_receiver "$1"; return 0; }; tmux send-keys -t dev -l "MERGE-RUNNER: $1 Reconcile, then fill the builder cap from startable work." 2>/dev/null; sleep 1; tmux send-keys -t dev Enter 2>/dev/null; }
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
      && git add docs/tasks.yaml && HK_MERGE_RUNNER=1 git commit -q -m "Board: $t landed as ${sha:0:8} (merge runner)"); then
    log "BOARD $t -> done (${sha:0:8})"
  else
    (cd "$REPO" && git checkout -q -- docs/tasks.yaml 2>/dev/null)
    log "flip_done $t FAILED - board left as is; needs reconcile"
  fi
}

# The work runner's own board flips (a dead dispatch back to todo, a dispatch to in-progress) need
# main to be safe to commit, and during back-to-back gates the work runner almost never sees that:
# on 2026-09-23 18:30-19:10 T-801 and T-512 waited to go back to todo while 19 tickets queued behind
# T-801 and dispatch sat at 0 of 6. Right after a landing - merge committed, no batch marker - is the
# one moment this runner KNOWS main is safe, so it lends it: one `work-runner.py --sync-board`, best
# effort, never able to fail the merge.
board_sync_now(){
  ( cd "$REPO" && python3 ops/work-runner.py --sync-board ) >>"$LOG" 2>&1 || true
}

# returns: 0 = handled (merged/skipped/flagged), 1 = transient (requeue + wait)
# IS MAIN ITSELF RED on this triage's own reds? Run on the clean main (a bulk rewound, a single merge
# aborted). 0 = yes, MAIN_RED_WHAT says on what; 1 = green, or nothing to check. A test that fails alone
# on main+branch and ALSO alone on main is main's defect: gating branch after branch only re-proves it
# (15:45 on 2026-09-22 three branches, 11:19-11:30 on 2026-09-24 T-858/T-802/T-803 - each blamed and
# charged an attempt for app-trace's T-475 colour check, which main failed until the deflake landed).
main_is_red(){
  MAIN_RED_WHAT=""
  # A CHECK red (lint / ui-unit, TRIAGE_CHECK): the same check on main alone. Red there = main's, the MAIN_RED hold.
  if [ -n "${TRIAGE_CHECK:-}" ]; then
    log "TRIAGE: is main itself red? running just $TRIAGE_CHECK alone on main"
    if ! check_probe >>"$LOG" 2>&1; then MAIN_RED_WHAT="just $TRIAGE_CHECK"; return 0; fi
    log "TRIAGE: main is green on just $TRIAGE_CHECK -> not main's"; return 1
  fi
  [ "${TRIAGE_KIND:-test}" = "test" ] || return 1
  if [ -n "${TRIAGE_FILTER:-}" ]; then
    log "TRIAGE: is main itself red? re-running the failing tests alone on main"
    # --no-tests=pass: a test the branch ADDED does not exist on main, and nextest's "no tests to run"
    # exit read as red - 09-24 10:15, T-870's own new test declared MAIN IS RED and never isolated.
    if ! ( cd "$REPO" && cargo nextest run --workspace --no-tests=pass -E "$TRIAGE_FILTER" ) >>"$LOG" 2>&1; then
      MAIN_RED_WHAT="$TRIAGE_FILTER"; return 0
    fi
    log "TRIAGE: main is green on them -> not main's"; return 1
  fi
  # The same question for browser specs (fog-of-war on 2026-09-22 16:22: red on main since
  # T-580 landed in the hand fast-forward, and the batch would have been isolated four times).
  if [ -n "${TRIAGE_SPECS:-}" ]; then
    log "TRIAGE: is main itself red? re-running the browser specs alone on main: $TRIAGE_SPECS"
    # REBUILD FIRST. The spec runner serves whatever `target/debug/hk` and `ui/dist` already exist
    # (ui/e2e/backend.mjs), and those were built from the BATCH tree by the gate that just failed.
    # At 13:47 on 2026-09-23 this step ran main's spec against the batch's UI bundle - which
    # carried the very tilecache.ts change surface-nav was red on - and declared MAIN IS RED,
    # re-queueing eight branches behind a defect that belonged to one of them. `just test-ui-e2e`
    # rebuilds both before it runs; this path must too, or its verdict is about the wrong tree.
    log "TRIAGE: rebuilding hk and ui/dist from main before the spec re-run"
    ( cd "$REPO" && cargo build -q -p hk-cli --bin hk && cd ui && npm run build ) >>"$LOG" 2>&1 \
      || log "TRIAGE: WARN rebuild failed; the spec re-run below may test the branch's artefacts"
    if ! ( cd "$REPO/ui" && npm run e2e -- $TRIAGE_SPECS ) >>"$LOG" 2>&1; then
      MAIN_RED_WHAT="browser spec(s) $TRIAGE_SPECS"; return 0
    fi
    log "TRIAGE: main is green on them -> not main's"; return 1
  fi
  return 1
}

# BISECT, NOT ISOLATE (supervisor for the user, 2026-09-24 19:37: four 1-by-1 isolations that day, 5-9 h of
# serial full gates each). When a batch's triaged test/spec fails alone and main is green on it, find the
# branch that breaks it with THAT TEST ALONE over halves of the batch (log2(n) targeted runs), confirm the
# last candidate is red ALONE, and only then blame it. Everything else goes back as one batch and still
# lands only through a full gate - nothing is excused. The candidates are persisted in isolate-remaining,
# so a restart mid-bisect re-queues them (startup). A bisect that names NO culprit re-queues the batch first as
# one batch (proven green, unprobed, then the last red subset) and isolates only when a later no-culprit red subset
# shares a member (by tip) with the recorded one.
# The probe for a CHECK red: exactly the gate suite that went red, over the whole workspace (as the merge gate runs
# it - HK_GATE_CRATES empty is --workspace). `just lint` is fmt + clippy --all-targets + ruff, a compile check, no test run.
check_probe(){ ( cd "$REPO" && HK_GATE_CRATES="" just "$TRIAGE_CHECK" ); }
# A VERIFIED REWIND (incident 2026-09-26 03:07): bisect_red's `git reset -q --hard "$base"` after probe 3 did not take
# (no reflog entry; its stderr went nowhere - likely another process's index.lock), the next probe gave up on "main
# moved", SUITE_BROKEN logged "rewound" without rewinding, and the probe merge e88d6238 (T-940) stayed on main UNGATED
# - the loop then skipped T-940 as "already merged". So: reset, CHECK HEAD, log git's own words, retry (a lock is
# brief); still off base = MAIN_DIRTY for a person, and $S/main-dirty stops every gate and merge until one clears it.
reset_to_base(){ # base why -> 0 = HEAD is base; 1 = it is not (flagged, alerted, $S/main-dirty written)
  local base=$1 why=$2 i err head
  for i in 1 2 3; do
    err=$(git -C "$REPO" reset -q --hard "$base" 2>&1)
    head=$(git -C "$REPO" rev-parse HEAD 2>/dev/null)
    [ "$head" = "$base" ] && return 0
    log "RESET: try $i of 3 to ${base:0:8} ($why) left HEAD at ${head:0:8}: ${err:-git said nothing}"
    [ "$i" -lt 3 ] && sleep "${RESET_RETRY_S:-3}"
  done
  printf 'base=%s\nhead=%s\nwhy=%s\nsince=%s\n' "$base" "$head" "$why" "$(date '+%Y-%m-%d %H:%M:%S')" > "$S/main-dirty"
  echo "$(date '+%m-%d %H:%M')  (main)  -  MAIN_DIRTY - main is at ${head:0:8}, not ${base:0:8} after '$why': an UNGATED commit sits on main; reset main to ${base:0:8} by hand, then rm $S/main-dirty (no gate or merge runs until then)" >> "$NEEDS"
  alert red "main left on an ungated commit" "$why: git reset --hard ${base:0:8} failed 3 times, main is at ${head:0:8}. ${err:-} No gate or merge runs until a person resets main and removes $S/main-dirty." --key "main-dirty"
  return 1
}
bisect_red(){ # base branch=sha... -> 0 = the triaged tests are RED on base + these, 1 = green, 2 = gave up
  # The SAME re-run main_is_red just answered "green" with on base, so the only difference between
  # the two verdicts is the branches merged here - by the tips recorded before the bisect began.
  local base=$1; shift; local t rc=1
  local head; head=$(git -C "$REPO" rev-parse HEAD)
  if [ "$head" != "$base" ]; then
    # Our own leftover probe (a reset that did not take) is ours to remove; anything else is a person's commit.
    if [ "$(git -C "$REPO" rev-parse "HEAD^1" 2>/dev/null)" = "$base" ] \
       && git -C "$REPO" log -1 --format=%s HEAD 2>/dev/null | grep -q ': bisect probe (automated, never kept)$'; then
      log "BISECT: HEAD ${head:0:8} is this runner's own leftover probe on ${base:0:8} - resetting it and going on"
      reset_to_base "$base" "leftover bisect probe ${head:0:8}" || return 2
    else
      log "BISECT: main moved off $base during the bisect - giving up, nothing reset"; return 2
    fi
  fi
  for t in "$@"; do
    if ! git -C "$REPO" merge -q --no-ff -m "Merge $(ticket_of "${t%%=*}") (${t%%=*}): bisect probe (automated, never kept)" "${t#*=}" >>"$LOG" 2>&1; then
      git -C "$REPO" merge --abort 2>/dev/null; reset_to_base "$base" "bisect: ${t%%=*} did not merge" || return 2
      log "BISECT: ${t%%=*} does not merge onto $base with the others - giving up"; return 2
    fi
  done
  if [ -n "${TRIAGE_CHECK:-}" ]; then
    check_probe >>"$LOG" 2>&1 || rc=0
  elif [ -n "${TRIAGE_FILTER:-}" ]; then
    ( cd "$REPO" && cargo nextest run --workspace --no-tests=pass -E "$TRIAGE_FILTER" ) >>"$LOG" 2>&1 || rc=0
  else
    ( cd "$REPO" && cargo build -q -p hk-cli --bin hk && cd ui && npm run build ) >>"$LOG" 2>&1 \
      || { reset_to_base "$base" "bisect: rebuild failed" || return 2; log "BISECT: rebuild failed on ${*%%=*} - giving up"; return 2; }
    ( cd "$REPO/ui" && npm run e2e -- $TRIAGE_SPECS ) >>"$LOG" 2>&1 || rc=0
  fi
  reset_to_base "$base" "after bisect probe $(printf '%s ' "${@%%=*}")" || return 2
  log "BISECT: base + $(printf '%s ' "${@%%=*}")-> $([ "$rc" = 0 ] && echo RED || echo green)"
  return $rc
}
# The probe verdicts, for the no-culprit re-queue: `green <branch=sha...>` per green probe, `red <branch=sha...>` per
# red one (the last red line is the smallest red subset), `gave-up` when a probe gave up. A file, because
# bisect_culprit runs in a $( ) subshell.
bisect_fact(){ # rc branch=sha...
  local r=$1; shift
  case "$r" in 0) echo "red $*" ;; 1) echo "green $*" ;; *) echo "gave-up $*" ;; esac >> "$S/bisect-facts"
}
bisect_culprit(){ # base branch=sha... -> echoes the one branch=sha red ALONE (twice), or nothing; leaves $S/bisect-facts
  local base=$1; shift; local cand=("$@") n r
  printf '%s ' "${@%%=*}" > "$S/isolate-remaining"
  echo "red $*" > "$S/bisect-facts"   # the batch gate itself
  while [ "${#cand[@]}" -gt 1 ]; do
    n=$(( ${#cand[@]} / 2 ))
    bisect_red "$base" "${cand[@]:0:$n}"; r=$?
    bisect_fact "$r" "${cand[@]:0:$n}"
    [ "$r" = 2 ] && return 0
    if [ "$r" = 0 ]; then cand=("${cand[@]:0:$n}"); else cand=("${cand[@]:$n}"); fi
  done
  # Blame needs the red on base + it ALONE twice: a halving that ended on the green side rests on
  # nothing else, and one red run cannot tell a defect from a test that is flaky even alone.
  bisect_red "$base" "${cand[0]}"; r=$?; bisect_fact "$r" "${cand[0]}"; [ "$r" = 0 ] || return 0
  bisect_red "$base" "${cand[0]}"; r=$?; bisect_fact "$r" "${cand[0]}"; [ "$r" = 0 ] && echo "${cand[0]}"
  return 0
}

# REMOTE MIRRORS (user, 2026-09-25 00:15): after every landing, main goes to each remote worker host's mirror
# ($HACKRIFF_OPS/hosts.json names them; each is a git remote of this repo), so a remote worker never starts from a
# stale base and the drift is visible. Only the LANDED HEAD (after the bulk marker is gone and the board synced), never --force, in the
# background - a slow or absent host never delays the next gate. A failure is logged; the next landing retries.
push_mirrors(){
  [ -s "$S/hosts.json" ] || return 0
  local sha h; sha=$(git -C "$REPO" rev-parse HEAD)
  for h in $(python3 -c 'import json, sys; print(" ".join(json.load(open(sys.argv[1]))))' "$S/hosts.json" 2>/dev/null); do
    ( if timeout 120 git -C "$REPO" push -q --no-verify "$h" "$sha:refs/heads/main" >/dev/null 2>&1; then log "PUSHED $h ${sha:0:8}"
      else log "PUSH FAILED $h ${sha:0:8} - host down or its mirror not a fast-forward; the next landing retries"; fi ) &
  done
}

rc_due(){ # 0 = run the release candidate now
  [ -e "$S/rc-requested" ] && return 0
  [ "$GATE_TIERS" = check ] || return 1      # under full every merge already runs the acceptance phase
  [ $((10#$(date +%H))) -ge "$RC_HOUR" ] || return 1
  [ "$(sed -n 's/^day=//p' "$S/rc-last" 2>/dev/null)" = "$(date +%Y%m%d)" ] && return 1
  return 0
}
run_rc(){ # main is ready (landed, clean, nothing staged) - checked by the caller
  local sha tag rc from failed steps seg last reds item r sp what ecode
  sha=$(git -C "$REPO" rev-parse HEAD); tag="rc-$(date +%Y%m%d)"
  # The DAY is the scheduled run's, taken at its start: an on-demand `just rc` neither uses up nor moves
  # the daily run (review: one ending after midnight skipped the next day's).
  [ -e "$S/rc-requested" ] || printf 'day=%s\n' "$(date +%Y%m%d)" > "$S/rc-last"
  rm -f "$S/rc-requested"
  if [ "$DRY_RUN" = "1" ]; then log "DRY-RUN would run the release candidate on main ${sha:0:8}"; return 0; fi
  printf 'sha=%s\nstarted=%s\n' "$sha" "$(date +%s)" > "$RCMARK"
  # Its own announcement, so hkpy.flow / cycletime start no merge gate here and credit its suites to none.
  log "RC gate (just gate --files crates/ --phase acceptance on main ${sha:0:8})…"
  from=$(( $(wc -l < "$LOG") ))
  limited just gate --files crates/ --phase acceptance; rc=$?
  # The gate stops at its first red suite, and acceptance-ci at its first red step (acceptance, then
  # e2e-harness): an RC still runs what the red left unrun - the flake path's own resume rule.
  failed=$(tail -n +"$from" "$LOG" | sed -n -E 's/^gate: just ([a-z0-9-]+) took [0-9]+s \(exit [1-9][0-9]*\)$/\1/p' | tail -1)
  if [ "$rc" -ne 0 ] && [ "$failed" = "acceptance-ci" ]; then
    seg=$(tail -n +"$from" "$LOG" | awk '/^gate: running just acceptance-ci/{buf=""; on=1} on{buf=buf"\n"$0} /^gate: just acceptance-ci took/{on=0} END{print buf}')
    steps=""; [ "$(printf '%s' "$seg" | grep -cE '^\s+Summary \[')" -lt 2 ] && steps="e2e-harness"
    limited just gate --files crates/ --phase acceptance --resume-after acceptance-ci ${steps:+--resume-steps $steps}
  fi
  printf 'sha=%s\nrc=%s\nat=%s\n' "$sha" "$rc" "$(date +%s)" > "$S/rc-result"
  rm -f "$RCMARK"
  if [ "$rc" -eq 0 ]; then
    git -C "$REPO" tag -f "$tag" "$sha" >/dev/null 2>&1
    log "RC GREEN ${sha:0:8} -> tagged $tag"
    return 0
  fi
  last=$(git -C "$REPO" describe --tags --match 'rc-*' --abbrev=0 "$sha" 2>/dev/null || echo "none yet")
  # Final failures only: the lines nextest repeats under its `Summary [` (a test that passed on a retry, or a
  # LEAK - which is a pass - is not there), in the runner's own red-line shape; and the browser runner's
  # `failed:` list.
  reds=$(tail -n +"$from" "$LOG" | awk '
      /^ +Summary \[/ {s=1; next}
      s && match($0, /^ +(TRY [0-9]+ )?(FAIL|SIG[A-Z]+|TIMEOUT|ABORT|LEAK-FAIL) \[[^]]*\] \([^)]*\) /) {
        n=split(substr($0, RSTART+RLENGTH), f, " "); if (n >= 2) print "rust " f[1] " " f[2]; next }
      s && !/^ +/ {s=0}
      /^e2e: .*failed: / {sub(/.*failed: /, ""); gsub(/,/, " "); print "spec " $0}' | sort -u)
  # A suite that went red with no named test (a build, a harness crash, a runner that died) is its own item.
  for ecode in acceptance-ci test-ui-e2e; do
    tail -n +"$from" "$LOG" | grep -qE "^gate: just $ecode took [0-9]+s \(exit [1-9]" || continue
    case "$ecode" in acceptance-ci) printf '%s\n' "$reds" | grep -q '^rust ' ;; *) printf '%s\n' "$reds" | grep -q '^spec ' ;; esac \
      || reds=$(printf '%s\nsuite %s\n' "$reds" "$ecode")
  done
  [ -z "$(printf '%s' "$reds" | tr -d '[:space:]')" ] && reds="suite ${failed:-unknown (rc $rc)}"
  printf '%s\n' "$reds" | grep -v '^$' | while read -r kind a b; do
    case "$kind" in
      rust)
        what=$(tail -n +"$from" "$LOG" | grep -A40 -F "$b" | grep -m1 -E 'panicked at|assertion' | sed 's/^ *//' | cut -c1-200)
        echo "$(date '+%m-%d %H:%M')  main@${sha:0:8}  (rc)  RC_RED P1 - $a $b red in the release candidate; last green: $last; first red tip: ${sha:0:8}; repro: cargo nextest run -p ${a%%::*} -E 'binary_id($a) & test(=$b)'; assertion: ${what:-see merge-runner.log}" >> "$NEEDS" ;;
      spec)
        for sp in $a $b; do
          what=$(tail -n +"$from" "$LOG" | awk -v sp="$sp" 'index($0, "▶ " sp){f=1} f && /AssertionError|Error:/{sub(/^ +/, ""); print; exit}' | cut -c1-200)
          echo "$(date '+%m-%d %H:%M')  main@${sha:0:8}  (rc)  RC_RED P1 - spec $sp red in the release candidate; last green: $last; first red tip: ${sha:0:8}; repro: cd ui && node e2e/run.mjs ${sp%.e2e.mjs}; assertion: ${what:-see merge-runner.log}" >> "$NEEDS"
        done ;;
      *) echo "$(date '+%m-%d %H:%M')  main@${sha:0:8}  (rc)  RC_RED P1 - suite $a red with no named test (build/harness/crash) in the release candidate; last green: $last; repro: just gate --files crates/ --phase acceptance" >> "$NEEDS" ;;
    esac
  done
  # Count reds, not lines: the browser runner lists every failed spec on ONE 'failed:' line (03:53: '1 red' for 4 items).
  log "RC RED ${sha:0:8} ($(printf '%s\n' "$reds" | awk '$1=="spec"{n+=NF-1; next} NF{n++} END{print n+0}') red) -> P1 items in $NEEDS; nothing un-lands"
  notify_coordinator "the release candidate at ${sha:0:8} is RED - P1 item(s) RC_RED in the attention file (last green $last); file them found_by rc-$(date +%Y%m%d)." "RC red - P1 tickets"
  return 0
}

suite_red_alone(){ # base branch-or-sha "tests/x.py::a tests/y.py::b" -> 0 when those pytest tests are red on base + it
  # pytest exits 1 (tests failed) or 2 (collection error) for a red; 4/5 (no such node / nothing collected) is not red -
  # a test the branch does not have cannot be red with it, and a test new in the batch is not main's red.
  local base=$1 b=$2 ids=$3 rc=0
  reset_to_base "$base" "suite probe ${b:-(base)}" || return 1
  if [ -z "$b" ] || git -C "$REPO" merge -q --no-ff -m "suite probe $b (never kept)" "$b" >>"$LOG" 2>&1; then
    ( cd "$REPO/py" && uv run --locked pytest -q -p no:cacheprovider $ids ) >>"$LOG" 2>&1; rc=$?
  else
    git -C "$REPO" merge --abort 2>/dev/null
  fi
  reset_to_base "$base" "after suite probe ${b:-(base)}" || return 1
  log "SUITE: ${b:-(base)} alone on ${base:0:8} -> pytest exit $rc"
  [ "$rc" = 1 ] || [ "$rc" = 2 ]
}
suite_split(){ # base "ids" name=sha... -> "RED name=sha" / "GREEN name=sha"; nothing when the ids are red on base itself
  local base=$1 ids=$2 b; shift 2
  printf '%s ' "${@%%=*}" > "$S/isolate-remaining"   # a runner killed mid-probe re-queues the whole batch (as bisect_culprit)
  suite_red_alone "$base" "" "$ids" && return 0
  for b in "$@"; do
    # Twice, as bisect_culprit: one red run cannot tell a defect from a test flaky even alone.
    if suite_red_alone "$base" "${b#*=}" "$ids" && suite_red_alone "$base" "${b#*=}" "$ids"; then echo "RED $b"; else echo "GREEN $b"; fi
  done
}

process(){
  local branch=$1 ticket; ticket=$(ticket_of "$branch")
  cd "$REPO" || return 1
  git rev-parse --verify "$branch" >/dev/null 2>&1 || { log "SKIP $branch: no such branch"; return 0; }
  [ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || { log "WAIT $branch: HEAD not on main"; return 1; }
  [ -e "$REPO/.git/MERGE_HEAD" ] && { log "WAIT $branch: a merge is already in progress"; return 1; }
  [ -e "$S/main-dirty" ] && { log "WAIT $branch: MAIN_DIRTY - main is not on a gated commit ($S/main-dirty)"; return 1; }
  git diff --quiet && git diff --cached --quiet || { log "WAIT $branch: main tree dirty (coordinator mid-commit)"; return 1; }
  local ahead; ahead=$(git rev-list --count "main..$branch" 2>/dev/null || echo 0)
  if [ "${ahead:-0}" -eq 0 ]; then log "SKIP $branch: nothing ahead of main (already merged?)"; return 0; fi

  local tip prev tries; tip=$(git rev-parse "$branch"); prev=$(failed_sha_of "$branch"); tries=$(attempts_of "$branch")
  if [ -n "$prev" ] && [ "$prev" = "$tip" ]; then
    log "SKIP $branch: UNCHANGED SINCE ITS GATE FAILURE ($tip) - needs a fix, not a re-queue"
    echo "$(date '+%m-%d %H:%M')  $branch  $ticket  UNCHANGED_SINCE_FAIL ($tip)" >> "$NEEDS"
    notify_coordinator "$ticket ($branch) was re-queued UNCHANGED since its gate failure - fix the branch first; it was NOT re-gated." "re-queued unchanged - not re-gated"
    return 0
  fi
  if [ "${tries:-0}" -ge "$MAX_ATTEMPTS" ]; then
    log "GIVE UP $branch: $tries gate attempts already (cap $MAX_ATTEMPTS) - escalating, no further automatic retries"
    echo "$(date '+%m-%d %H:%M')  $branch  $ticket  GAVE_UP after $tries attempts - NEEDS A PERSON" >> "$NEEDS"
    notify_coordinator "$ticket ($branch) has now FAILED $tries gate attempts; the runner has GIVEN UP and will not retry it." "needs a person - $ticket gate attempts spent"
    return 0
  fi
  if [ "$DRY_RUN" = "1" ]; then log "DRY-RUN would merge $branch ($ticket, $ahead ahead)"; return 0; fi

  log "MERGE start $branch ($ticket, $ahead commits ahead)"
  if ! git merge --no-ff --no-commit "$branch" >>"$LOG" 2>&1; then
    git merge --abort 2>/dev/null || true
    log "CONFLICT $branch -> flag for AI"
    echo "$(date '+%m-%d %H:%M')  $branch  $ticket  CONFLICT" >> "$NEEDS"; notify_coordinator "$ticket ($branch) hit a MERGE CONFLICT with main." "merge conflict - fix run"; return 0
  fi
  log "GATE $branch (just gate-merge${GATE_PHASE:+ $GATE_PHASE}; may take 15-25 min)…"
  GATE_T0=$SECONDS
  local gate_line rc; gate_line=$(( $(wc -l < "$LOG") ))
  limited just gate-merge $GATE_PHASE; rc=$?
  # Same triage as a bulk (flake_retry): a single branch's red used to go straight to
  # GATE_FAIL and burn one of its MAX_ATTEMPTS on a load flake it never touched - task-gatefix
  # spent its second and last attempt that way on 2026-09-22 (api_contract tile_shadow…).
  if [ "$rc" -ne 0 ]; then flake_retry "" "$gate_line" "$ticket" "just gate-merge $GATE_PHASE"; rc=$?; fi
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
    if ! HK_MERGE_RUNNER=1 git commit -m "Merge $ticket ($branch): gate passed (automated merge, no AI)" >>"$LOG" 2>&1; then
      log "COMMIT REFUSED for $branch (pre-commit hook or hook failure) - NOT merged"
      git merge --abort 2>/dev/null || true
      echo "$(date '+%m-%d %H:%M')  $branch  $ticket  COMMIT_REFUSED" >> "$NEEDS"
      notify_coordinator "$ticket ($branch) gated GREEN but its merge COMMIT was refused (see the log; usually a malformed docs/tasks.yaml). main is untouched." "merge commit refused"
      return 0
    fi
    log "MERGED $branch ✓"
    record_landed "$branch"
    clear_attempts "$branch"
    echo "$(date '+%m-%d %H:%M')  $branch  $ticket  MERGED" >> "$DONELOG"
    board_sync_now
    push_mirrors       # the landed tip, board sync included (a mirror behind by the sync would read as drift)
    local wt; wt=$(worktree_of "$branch")
    if [ -n "$wt" ] && [ "$(cd "$wt" && pwd -P)" != "$(cd "$REPO" && pwd -P)" ]; then
      git worktree remove "$wt" --force 2>>"$LOG" && log "worktree removed: $wt"
    fi
    local q; q=$(grep -vcE '^[[:space:]]*(#|$)' "$QUEUE" 2>/dev/null || echo 0)
    notify_ok "MERGED $ticket ($branch); queue now $q waiting." "1 landed · gate $(( (SECONDS - ${GATE_T0:-$SECONDS} + 30) / 60 )) min · queue now $q waiting" "$branch"
  else
    git merge --abort 2>/dev/null || true
    if main_is_red; then
      # Not this branch's red: no attempt charged, re-queued by the caller; an isolation stops here.
      log "TRIAGE: MAIN IS RED on: $MAIN_RED_WHAT -> $branch re-queued, no attempt charged; queue the fix"
      echo "$(date '+%m-%d %H:%M')  $branch  $ticket  MAIN_RED - $MAIN_RED_WHAT fail(s) on main itself; fix main, the branch is re-queued behind the fix" >> "$NEEDS"
      notify_coordinator "main itself fails $MAIN_RED_WHAT - $ticket ($branch) is re-queued, not blamed; queue a fix for main." "main is red - fix for main needed"
      MAIN_RED_STOP=1
      # Parked until main moves: re-gating it against the same red main only re-proves the red
      # (review, 2026-09-24: a lone branch re-queued here re-gated every tick until main was fixed).
      echo "$branch $(git -C "$REPO" rev-parse HEAD) $tip" >> "$S/main-red-parked"
      return 1
    fi
    # Main passed it alone just now, but the same spec failed alone on ANOTHER branch within a day:
    # an intermittent defect on main, not this branch's (canvas-journey, 2026-09-24). Held like a main
    # red - no attempt, parked until main or the branch moves - and a deflaker/ticket requested once.
    # Once per branch tip: a second main-side verdict on the same tip is charged like any red (its
    # worker gets the fix run) - two different real defects in one spec must not park a branch for a day.
    local side=""; grep -qx "$branch $tip" "$S/main-side-seen" 2>/dev/null || side=$(main_side_of "$branch" | sed 's/^main-side //')
    if [ -n "$side" ]; then
      echo "$branch $tip" >> "$S/main-side-seen"
      local what; what=$(tail -n +"$gate_line" "$LOG" | grep -m1 -E 'AssertionError|panicked at' | sed 's/^ *//' | cut -c1-240)
      log "TRIAGE: MAIN-SIDE $(echo $side) -> $branch held, no attempt charged: the spec fails alone on 2+ branches in 24 h"
      echo "$(date '+%m-%d %H:%M')  $branch  $ticket  BLOCKED_ON_SPEC - $(echo $side) - fails alone on 2+ different branches in 24 h: a MAIN-side defect, not this branch's. Assertion: ${what:-see merge-runner.log}. Request: a deflaker or a ticket for main (evidence: just flakes; the branches above). $branch is held and re-queued automatically when main moves." >> "$NEEDS"
      notify_coordinator "main-side $(echo $side | cut -c1-120) - $ticket ($branch) held, not blamed; assertion: ${what:-see log}. Needs a deflaker or a ticket for main." "main-side defect - deflaker/ticket needed"
      echo "$branch $(git -C "$REPO" rev-parse HEAD) $tip" >> "$S/main-red-parked"
      return 1
    fi
    record_attempt "$branch" "$tip"
    log "GATE FAILED $branch (attempt $((tries+1))/$MAX_ATTEMPTS, tip $tip) -> abort + flag for AI"
    echo "$(date '+%m-%d %H:%M')  $branch  $ticket  GATE_FAIL" >> "$NEEDS"; notify_coordinator "$ticket ($branch) FAILED the merge gate: $(echo ${TRIAGE_SPECS:-} ${TRIAGE_FILTER:-} ${TRIAGE_WHAT:-} | cut -c1-300)" "gate failed - fix run"
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
    # A claim with a host runs on that remote host: it shares neither this box's CPU nor its ports, so the gate
    # never waits for it (2026-09-25 01:2x: 'gating a contended box - 10 worker(s) running' counted node2's three).
    print(sum(1 for c in d.values() if c.get("state") == "running" and not c.get("host")))
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
rm -f "$GATEWANT" "$RCMARK"   # markers from a previous run must not outlive it

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
  local b a why
  : > "$S/pm-held"   # task-pm-* branches this pass held for a person; the loop re-queues them
  for b in "$@"; do
    git -C "$REPO" rev-parse --verify "$b" >/dev/null 2>&1 || { log "SKIP $b: no such branch"; continue; }
    a=$(git -C "$REPO" rev-list --count "main..$b" 2>/dev/null || echo 0)
    [ "${a:-0}" -eq 0 ] && { log "SKIP $b: nothing ahead of main (already merged?)"; continue; }
    # HELD FOR REVIEW (supervisor 2026-09-25 12:24, incident T-955): a queued branch awaiting a review verdict outside
    # the runner stays queued and is never gated while $S/review-hold/<branch> exists - the coordinator writes it
    # when it sends the branch to review and removes it after the verdict. At 12:20 T-955's tip moved (a conflict
    # fix) while it waited for its Opus review; the moved tip re-gated and landed review-FAILED code.
    if [ -e "$S/review-hold/$b" ]; then
      if ! cmp -s "$S/review-hold/$b" "$S/review-hold/.said-$b"; then   # said once per marker text (a subshell: no variable survives)
        log "REVIEW HOLD $b: $(head -c 160 "$S/review-hold/$b" | tr '\n' ' ')- stays queued, not gated, until $S/review-hold/$b is removed"
        cp "$S/review-hold/$b" "$S/review-hold/.said-$b"
      fi
      echo "$b" >> "$S/pm-held"; continue
    fi
    # THE PIPELINE MANAGER'S SCOPE CHECK (user, 2026-09-23 17:30: "a lot of changes is fine; not
    # weird changes that aren't warranted - stick to the directive"; hkpy.pmbudget, tested). A
    # task-pm-* branch merges only with a `Serves:` line (an experiment, an incident, a user ask,
    # or a MEASURED cost) and only inside pipeline paths - product code or the board holds it.
    # Volume is reported, never capped. A held branch is re-queued, said once per tip here and in
    # the attention file, and alerted once; a person releases it with `just pm-budget release`.
    case "$b" in task-pm-*)
      why=$(cd "$REPO" && uv run --locked --project py python -m hkpy.pmbudget check "$b" --base main 2>&1 | tail -1)
      if [ "$?" -ne 0 ] || printf '%s' "$why" | grep -q ' HELD - '; then
        tip=$(git -C "$REPO" rev-parse --short "$b" 2>/dev/null)
        case " ${PM_HELD_SAID:-} " in *" $b@$tip "*) ;; *)
          PM_HELD_SAID="${PM_HELD_SAID:-} $b@$tip"
          log "PM-BUDGET HELD $b: ${why#pm-budget $b: HELD - }"
          printf '%s  %s  %s  PM-BUDGET(held: %s)\n' "$(date '+%m-%d %H:%M')" "$b" "$(ticket_of "$b")" "${why#pm-budget $b: HELD - }" >> "$NEEDS"
          alert amber "pipeline branch held by the code budget" "$b: ${why#pm-budget $b: HELD - }. Release: just pm-budget release $b" --key "pm-budget:$b" ;;
        esac
        echo "$b" >> "$S/pm-held"; continue
      fi ;;
    esac
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
# The one-solo-pass rule (user decision 2026-09-24 14:20, knob FLAKE_SOLO_ONE, an experiment whose
# rollback is FLAKE_SOLO_ONE=0 = the twice rule): after the FIRST isolated pass, a red whose flake
# ledger already shows every test passing alone >= 2 times and never failing alone in 7 d is accepted
# without the second run. Prints `solo-ok N`; exit 1 (twice rule) when off, unknown, or not qualified.
# A merge that changes the code judging its own red (the ledger, the runner) must not be excused by it
# (review, 2026-09-24). Compared against the last gated commit - a bulk's base=, else HEAD for a staged merge.
judge_changed(){
  local ref=HEAD; [ -f "$BULKMARK" ] && ref=$(sed -n 's/^base=//p' "$BULKMARK" | head -1)
  ! git -C "$REPO" diff --quiet "${ref:-HEAD}" -- py/hkpy py/pyproject.toml py/uv.lock ops/merge-runner.sh 2>/dev/null
}
solo_ok(){
  [ "${FLAKE_SOLO_ONE:-0}" = 1 ] || return 1
  judge_changed && return 1
  ( cd "$REPO" && uv run --locked --project py python -m hkpy.flakes --solo-ok "$@" ) 2>/dev/null
}
# Supervisor for the user, 2026-09-24 14:55: a spec that fails alone on >= 2 DIFFERENT branches in 24 h
# is main's defect (canvas-journey was pinned on three merges; each fail-alone counted as a branch
# defect, so the deflake path never fired). Prints the ledger's `main-side TEST: other branches` lines.
main_side_of(){ # branch  (runs after the merge is aborted: main's own ledger code decides)
  local names="${TRIAGE_SPECS:-${TRIAGE_TESTS:-}}"
  [ -n "$names" ] || return 0
  ( cd "$REPO" && uv run --locked --project py python -m hkpy.flakes --main-side $names --branch "$1" ) 2>/dev/null
}
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

_flake_retry(){ # base gate_log_start_line tickets [retry_cmd] -> exit 0 if the gate may land
  # THE USER'S RULE (2026-09-23): a red test that passes ALONE TWICE is a load flake and its suite
  # passes on that evidence - no phase re-run, no full-gate re-run; the gate resumes after the
  # suite that stopped it, so nothing that never ran is skipped. Failing alone (either isolated
  # run) is unchanged: a real red - hold/isolate, the MAIN-IS-RED check with the rebuild. Every
  # acceptance is recorded in flaky.jsonl (py/hkpy/flakes.py counts them; the 3rd in 7 days of one
  # test files a deflake request the work runner dispatches) and alerted - never a silent pass.
  # retry_cmd defaults to `just gate --base $base` (a bulk, already committed on main); the
  # single-branch path passes `just gate-merge`, because its merge is still STAGED.
  local base=$1 from=$2 tickets=$3 retry=${4:-"just gate --base $base $GATE_PHASE"} tests filter t0
  # nextest prints `FAIL [` for a plain failure and `TRY n FAIL [` once .config/nextest.toml
  # gives a test retries (T-841); a test that passed on a retry prints `FLAKY` and is not red.
  # Every way nextest reports a red test - a crash (SIGSEGV/SIGABRT/...), a TIMEOUT, a leak - not
  # only `FAIL [`: with fail-fast off, a flaky test and a crashing one can be red in the same run,
  # and re-running only the first would accept the second (review, 2026-09-23).
  tests=$(tail -n +"$from" "$LOG" | grep -E '^\s+(TRY [0-9]+ )?(FAIL|SIG[A-Z]+|TIMEOUT|ABORT|LEAK-FAIL) \[' | awk '{print $NF}' | sort -u)
  TRIAGE_KIND="test"
  # This triage's own reds only: at 10:47 on 2026-09-24 the MAIN-IS-RED check re-ran the previous
  # triage's Rust filter (an accepted flake) instead of the browser spec that had just gone red.
  TRIAGE_FILTER=""; TRIAGE_SPECS=""; TRIAGE_WHAT=""; TRIAGE_TESTS=""; TRIAGE_CHECK=""; TRIAGE_ALONE_FIRST=0; FLAKE_PASSES=2; FLAKE_SOLO_S=0
  TRIAGE_T0=$(date '+%Y-%m-%dT%H:%M:%S')   # the red's own time: flakes.py matches its record to it
  # The browser tier (ui/e2e/run.mjs) reports its reds on one summary line, not as nextest FAIL
  # lines: `e2e: 11/13 files passed in 662.5 s (backend 2.9 s); failed: fog-of-war.e2e.mjs, ...`.
  local specs; specs=$(tail -n +"$from" "$LOG" | grep -E '^e2e: [0-9]+/[0-9]+ files passed .*; failed: ' | tail -1 | sed 's/.*failed: //' | tr -d ',')
  if [ -z "$tests" ] && [ -n "$specs" ]; then
    TRIAGE_SPECS="$specs"   # try_bulk re-runs these on main alone if this batch is red
    log "TRIAGE: browser specs red: $specs - re-running them alone"
    t0=$SECONDS
    if ( cd "$REPO/ui" && npm run e2e -- $specs ) >>"$LOG" 2>&1; then
      local solo; if solo=$(solo_ok $specs); then
        FLAKE_PASSES=1; FLAKE_SECOND_S=0; FLAKE_SOLO_S=$((SECONDS - t0))
        log "TRIAGE: accepted after one solo pass (ledger: ${solo#solo-ok } alone-passes)"
        flake_accept spec "$specs" "$from" "$tickets" "$retry"; return $?
      fi
      log "TRIAGE: first isolated run passed - running them alone once more (the rule is twice)"
      t0=$SECONDS
      if ( cd "$REPO/ui" && npm run e2e -- $specs ) >>"$LOG" 2>&1; then
        FLAKE_SECOND_S=$((SECONDS - t0)); flake_accept spec "$specs" "$from" "$tickets" "$retry"; return $?
      fi
      log "TRIAGE: a browser spec FAILS alone on the second isolated run -> flaky even alone: a real defect, not a load flake"
      return 1
    fi
    log "TRIAGE: a browser spec FAILS alone -> a real defect in this merge"
    TRIAGE_ALONE_FIRST=1; return 1
  fi
  # pytest (the `py` suites inside `just test`) prints `FAILED tests/x.py::name` - not a nextest line,
  # so it takes the suite path (hold for a fix), but the alarm names it: at 09:42 and 09:44 on
  # 2026-09-24 a board-check red was announced as "lint/build/ui-unit", the wrong place to look.
  local py; py=$(tail -n +"$from" "$LOG" | sed -n -E 's/^(FAILED|ERROR) (tests\/[^ ]+).*/\2/p' | sort -u | tr '\n' ' ')
  TRIAGE_WHAT=${py:+"pytest red: ${py% }"}
  # A red with no named test in `just lint` (fmt/clippy/ruff - a compile error) or `just test-ui` is a CHECK red:
  # try_bulk bisects the batch by that check alone (bisect_red). 2026-09-25 12:48-13:07: seven batches in a row
  # went red on clippy (t844 x t989 in classify.rs, t940's presence_intervals()) and were re-queued whole each time.
  if [ -z "$tests" ] && [ -z "$py" ]; then
    case "$(tail -n +"$from" "$LOG" | sed -n -E 's/^gate: just ([a-z0-9-]+) took [0-9]+s \(exit [1-9][0-9]*\)$/\1/p' | tail -1)" in
      lint) TRIAGE_CHECK=lint; TRIAGE_WHAT="just lint" ;;
      test-ui) TRIAGE_CHECK=test-ui; TRIAGE_WHAT="just test-ui" ;;
    esac
  fi
  [ -z "$tests" ] && { TRIAGE_KIND="suite"; log "TRIAGE: no FAIL lines found (${TRIAGE_WHAT:-lint/build/ui-unit failure}) - not a flake candidate"; return 1; }
  filter=""; for t in $tests; do filter="${filter:+$filter | }test(${t##*::})"; done
  TRIAGE_FILTER="$filter"   # try_bulk re-runs the same set on main alone if this batch is red
  TRIAGE_TESTS="$(echo $tests)"   # the full nextest names - the flake ledger's keys (main_side_of)
  log "TRIAGE: re-running the failing tests alone: $(echo $tests | tr '\n' ' ')"
  t0=$SECONDS
  if ( cd "$REPO" && HK_E2E_REQUIRE_SYNTH=1 HK_REQUIRE_FIXTURES=1 cargo nextest run --workspace -E "$filter" ) >>"$LOG" 2>&1; then
    local solo; if solo=$(solo_ok $tests); then
      FLAKE_PASSES=1; FLAKE_SECOND_S=0; FLAKE_SOLO_S=$((SECONDS - t0))
      log "TRIAGE: accepted after one solo pass (ledger: ${solo#solo-ok } alone-passes)"
      flake_accept rust "$(echo $tests | tr '\n' ' ')" "$from" "$tickets" "$retry"; return $?
    fi
    log "TRIAGE: first isolated run passed - running them alone once more (the rule is twice)"
    t0=$SECONDS
    # The gate's strict env (test-rust / acceptance / e2e-harness export it): alone, a missing
    # fixture or synth generator would SKIP-and-pass instead of failing as it did in the gate.
    if ( cd "$REPO" && HK_E2E_REQUIRE_SYNTH=1 HK_REQUIRE_FIXTURES=1 cargo nextest run --workspace -E "$filter" ) >>"$LOG" 2>&1; then
      FLAKE_SECOND_S=$((SECONDS - t0)); flake_accept rust "$(echo $tests | tr '\n' ' ')" "$from" "$tickets" "$retry"; return $?
    fi
    log "TRIAGE: a test FAILS alone on the second isolated run -> flaky even alone: a real defect, not a load flake"
    return 1
  fi
  log "TRIAGE: a test FAILS alone -> a real defect in this merge"
  TRIAGE_ALONE_FIRST=1; return 1
}

# The suite that stopped the gate passes on the isolation evidence; run what it never reached.
flake_accept(){ # kind names gate_log_start_line tickets retry_cmd -> rc of the resumed remainder
  local kind=$1 names=$2 from=$3 tickets=$4 retry=$5 failed steps="" old saved rc seg
  # The gate attempt's own output only: stop at the first TRIAGE line, before the isolated re-runs
  # print their own nextest summaries into the same log (review, 2026-09-23: counted, they made a
  # red in `acceptance` look as if `e2e-harness` had run too).
  local att; att=$(tail -n +"$from" "$LOG" | awk '/^\[[0-9-]+ [0-9:]+\] TRIAGE:/{exit} {print}')
  if [ "$kind" = rust ]; then
    local counted rerun
    counted=$(printf '%s\n' "$att" | grep -E '^ *Summary \[' | grep -oE '[0-9]+ (failed|timed out)' | awk '{s+=$1} END{print s+0}')
    rerun=$(printf '%s\n' $names | grep -c .)
    if [ "${counted:-0}" -gt "${rerun:-0}" ]; then
      log "TRIAGE: they PASS alone $([ "${FLAKE_PASSES:-2}" = 1 ] && echo once || echo twice), but the stopped run counted $counted failure(s) and only $rerun were re-run alone -> not accepted; a real red until every failure is accounted for"
      return 1
    fi
  fi
  failed=$(tail -n +"$from" "$LOG" | sed -n -E 's/^gate: just ([a-z0-9-]+) took [0-9]+s \(exit [1-9][0-9]*\)$/\1/p' | tail -1)
  if [ -z "$failed" ]; then
    # Cannot tell which suite stopped the gate: do the old, safe thing (a full retry).
    log "TRIAGE: they PASS alone $([ "${FLAKE_PASSES:-2}" = 1 ] && echo once || echo twice), but the stopped suite is not in the log -> full retry (the old path)"
    printf '{"ts":"%s","tests":"%s","batch":"%s","load_before":"%s","passes_alone":%s,"accepted":false}\n' "${TRIAGE_T0:-$(date '+%Y-%m-%dT%H:%M:%S')}" "$names" "$tickets" "$(uptime | sed 's/.*load averages*: *//')" "${FLAKE_PASSES:-2}" >> "$FLAKY"
    limited $retry; return $?
  fi
  # acceptance-ci runs two nextest steps (acceptance, then e2e-harness); a red in the first
  # leaves the second unrun - count the nextest summaries the stopped suite printed.
  if [ "$failed" = "acceptance-ci" ]; then
    seg=$(printf '%s\n' "$att" | awk '/^gate: running just acceptance-ci/{buf=""; on=1} on{buf=buf"\n"$0} /^gate: just acceptance-ci took/{on=0} END{print buf}')
    [ "$(printf '%s' "$seg" | grep -cE '^\s+Summary \[')" -lt 2 ] && steps="e2e-harness"
  fi
  # What the OLD rule would have re-run: every suite of this attempt for a Rust red (a full gate
  # retry), the acceptance phase for a browser red. The suites after the stopped one run under
  # both rules, so the saving is the old re-run minus the second isolated run.
  if [ "$kind" = spec ]; then
    old=$(tail -n +"$from" "$LOG" | sed -n -E 's/^gate: just (acceptance-ci|test-ui-e2e) took ([0-9]+)s.*/\2/p' | awk '{s+=$1} END{print s+0}')
  else
    old=$(tail -n +"$from" "$LOG" | sed -n -E 's/^gate: just [a-z0-9-]+ took ([0-9]+)s.*/\1/p' | awk '{s+=$1} END{print s+0}')
  fi
  saved=$(( old - ${FLAKE_SECOND_S:-0} )); [ "$saved" -lt 0 ] && saved=0
  log "TRIAGE: they PASS alone $([ "${FLAKE_PASSES:-2}" = 1 ] && echo once || echo twice) -> accepted as a load flake (the user's rule): just $failed passes on that evidence; resuming the gate after it${steps:+ (+ $steps, never run)} - saves ~$((saved / 60)) min over the old retry"
  printf '{"ts":"%s","tests":"%s","batch":"%s","load_before":"%s","passes_alone":%s,"accepted":true,"kind":"%s","suite":"%s","saved_s":%s,"solo_saved_s":%s}\n' "${TRIAGE_T0:-$(date '+%Y-%m-%dT%H:%M:%S')}" "$names" "$tickets" "$(uptime | sed 's/.*load averages*: *//')" "${FLAKE_PASSES:-2}" "$kind" "$failed" "$saved" "${FLAKE_SOLO_S:-0}" >> "$FLAKY"
  alert amber "flake accepted" "$names went red in \`just $failed\` for ($tickets) and passed alone $([ "${FLAKE_PASSES:-2}" = 1 ] && echo "once (one-solo-pass rule)" || echo twice); the batch goes on without a re-run (~$((saved / 60)) min saved). Counted in flaky.jsonl - the 3rd in 7 days spawns a deflaker." --key "flake-accept:$(echo "$names" | cut -c1-60)"
  local resumed; resumed=$(( $(wc -l < "$LOG") + 1 ))
  limited $retry --resume-after "$failed" ${steps:+--resume-steps $steps}; rc=$?
  [ "$rc" -eq 0 ] && log "TRIAGE: resumed gate PASSED" || log "TRIAGE: resumed gate FAILED -> a red in a suite that had not run yet"
  # That red gets the same triage, once (2026-09-24 10:47: a test-ui-e2e red on fog-of-war.e2e.mjs,
  # which passes alone 4 times in 7 days, went straight to isolating 11 branches with no re-run).
  if [ "$rc" -ne 0 ] && [ "${FLAKE_DEPTH:-0}" -lt 1 ]; then
    local FLAKE_DEPTH=1
    _flake_retry "" "$resumed" "$tickets" "$retry"; rc=$?
  fi
  return $rc
}

# SUITE_BROKEN: rewound, the batch back in order and held until the queue changes (a fix). try_bulk's own locals
# (branches, gated_sig, tickets, base) - for a red no probe can split: no named test, or a CHECK bisect that gave up.
suite_broken_hold(){
  for b in "${branches[@]}"; do echo "$b" >> "$QUEUE"; done
  printf '%s' "$gated_sig" > "$S/suite-broken"
  local where="rewound to $base"; [ "$(git -C "$REPO" rev-parse HEAD)" = "$base" ] || where="main is NOT on $base (not rewound)"
  log "BULK gate FAILED without a test FAIL (${TRIAGE_WHAT:-lint/build/ui-unit}) -> $where; batch re-queued in order, NOT isolated - main+batch needs a fix"
  echo "$(date '+%m-%d %H:%M')  (bulk)  $tickets  SUITE_BROKEN - no test FAIL; ${TRIAGE_WHAT:-lint/build/ui-unit} red on main+batch; fix and queue the fix, the batch is re-queued behind it" >> "$NEEDS"
  notify_coordinator "batch ($tickets) failed WITHOUT a test failure - ${TRIAGE_WHAT:-lint/build/ui-unit} is red on main+batch; fix that first, the batch is re-queued." "main+batch broken - fix needed"
  if [ "$(git -C "$REPO" rev-parse HEAD)" = "$base" ]; then rm -f "$BULKMARK"; fi
}
# MAIN_DIRTY mid-triage (reset_to_base failed): try_bulk's locals. The batch goes back in order, the marker stays (main
# carries ungated commits), and the loop holds on $S/main-dirty - no isolation onto a main that is not base.
bulk_dirty_stop(){
  [ -e "$S/main-dirty" ] || return 1
  rm -f "$S/isolate-remaining"
  for b in "${branches[@]}"; do echo "$b" >> "$QUEUE"; done
  log "BULK gate FAILED -> main NOT rewound to $base (MAIN_DIRTY); batch re-queued in order, nothing gates until a person clears $S/main-dirty"
  return 0
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
  # Each branch is merged BY ITS TIP, and `gated` keeps name=tip: a branch pushed while the batch gates must not be
  # what the hold, the bisect or the pytest probe name (2026-09-25 06:51: task-t943 moved 820bd656 -> 09d0a797 at
  # 06:51:40, mid-gate; the hold recorded the fixed tip it never gated and would have held the fix).
  local merged=() gated=() skipped="" tip
  for b in "${branches[@]}"; do
    tip=$(git -C "$REPO" rev-parse "$b")
    if git -C "$REPO" merge --no-ff -m "Merge $(ticket_of "$b") ($b): batch, gated together (automated, no AI)" "$tip" >>"$LOG" 2>&1; then
      merged+=("$b"); gated+=("$b=$tip")
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
  local gated_sig; gated_sig=$(for b in "${gated[@]}"; do printf '%s@%s\n' "${b%%=*}" "$(git -C "$REPO" rev-parse --short "${b#*=}")"; done | sort | tr '\n' ' ')
  tickets=""
  for b in "${branches[@]}"; do tickets="$tickets $(ticket_of "$b")"; done
  tickets="${tickets# }"
  after=$(git -C "$REPO" rev-parse HEAD)
  echo "after=$after" >> "$BULKMARK"
  log "BULK gate (just gate --base $base${GATE_PHASE:+ $GATE_PHASE} over ${#branches[@]} merged branches; may take 15-25 min)…"
  GATE_T0=$SECONDS
  # $(( )) strips the leading spaces macOS `wc -l` prints; `tail -n +"   381417"` is an
  # "illegal offset", prints nothing, and flake_retry then saw "no FAIL lines" on every red
  # gate it was ever given (2026-09-22 13:55: one flake -> 14 branches isolated).
  local gate_line; gate_line=$(( $(wc -l < "$LOG") ))
  limited just gate --base "$base" $GATE_PHASE; rc=$?
  # TRIAGE BEFORE ISOLATING. A red batch used to mean "rewind and re-gate every branch alone" -
  # 22 branches x 50 min on 2026-09-22, for one load-sensitive test no branch had touched. Now the
  # failing tests are re-run ALONE first (seconds to minutes); if they pass alone it is a load flake,
  # recorded in flaky.jsonl, and the whole gate is retried ONCE (workers are bounded, so the
  # gate's reserved cores are the gate's - nothing is suspended). Only a test that fails alone,
  # or a second red gate, still isolates.
  if [ "$rc" -ne 0 ] && [ "$GATE_TIMED_OUT" = 1 ]; then
    # A timed-out gate proves nothing about any test: treat it as a suite-wide red - rewind,
    # re-queue the batch once, hold until the queue changes - and say so where a person looks.
    TRIAGE_KIND="suite"; TRIAGE_FILTER=""; TRIAGE_SPECS=""; TRIAGE_CHECK=""; TRIAGE_WHAT="gate timed out"
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
    board_sync_now
    # Only now: while the bulk marker stood, a killed runner's startup still rewinds this batch (review, 2026-09-25).
    push_mirrors
    local q; q=$(grep -vcE '^[[:space:]]*(#|$)' "$QUEUE" 2>/dev/null || echo 0)
    notify_ok "MERGED batch ($tickets); queue now $q waiting." "${#branches[@]} landed · gate $(( (SECONDS - ${GATE_T0:-$SECONDS} + 30) / 60 )) min · queue now $q waiting" "${branches[@]}"
    return 0
  fi
  if [ "$(git -C "$REPO" rev-parse HEAD)" = "$after" ]; then
    if ! reset_to_base "$base" "bulk gate red"; then bulk_dirty_stop; return 0; fi
    # A red with NO test FAIL line is lint, a build error or the UI unit step - a property of
    # main+batch as a whole that every isolated gate would reproduce (2026-09-22 14:34: a
    # TypeScript type error on main itself; isolating 6 branches would have been 6 identical
    # reds, 6 attempt-ledger strikes and ~90 min). So: rewind, put the batch BACK in the queue
    # in order, flag it once, and wait for a fix to be queued - never isolate.
    # A pytest red names its tests: find the branch that breaks them ALONE (seconds per branch) instead of holding
    # the whole batch for a person to find it - twice by hand (2026-09-24 18:12-18:15, 2026-09-25 04:43).
    if [ "${TRIAGE_KIND:-test}" = "suite" ] && [ "${TRIAGE_WHAT#pytest red: }" != "${TRIAGE_WHAT:-}" ] && [ "${#branches[@]}" -ge 2 ]; then
      local culprits=() rest=() tips=("${gated[@]}") b kind
      while read -r kind b; do
        [ "$kind" = RED ] && culprits+=("$b"); [ "$kind" = GREEN ] && rest+=("${b%%=*}")
      done < <(suite_split "$base" "${TRIAGE_WHAT#pytest red: }" "${tips[@]}")
      rm -f "$S/isolate-remaining"
      bulk_dirty_stop && return 0
      if [ "${#culprits[@]}" -gt 0 ] && [ "${#rest[@]}" -gt 0 ]; then
        { printf '%s\n' "${rest[@]}"; cat "$QUEUE" 2>/dev/null; } > "$QUEUE.tmp" && mv "$QUEUE.tmp" "$QUEUE"
        for b in "${culprits[@]}"; do
          record_attempt "${b%%=*}" "${b#*=}"; b=${b%%=*}
          log "GATE FAILED $b (its own ${TRIAGE_WHAT} - red twice with it alone on $base, green on base) -> flag for AI"
          echo "$(date '+%m-%d %H:%M')  $b  $(ticket_of "$b")  GATE_FAIL" >> "$NEEDS"
          notify_coordinator "$(ticket_of "$b") ($b) breaks ${TRIAGE_WHAT} on its own (run with each batch branch alone); the rest of the batch is re-queued first." "gate failed - fix run"
        done
        log "SUITE: ${#culprits[@]} culprit(s) set aside (${culprits[*]%%=*}); ${#rest[@]} branch(es) re-queued first as one batch"
        rm -f "$BULKMARK"
        return 0
      fi
      log "SUITE: no split (${#culprits[@]} red alone of ${#branches[@]}, or red on base) -> the batch is held as before"
    fi
    # A CHECK red (just lint / just test-ui) over 2+ branches is not held whole: main_is_red, then bisect, below.
    if [ "${TRIAGE_KIND:-test}" = "suite" ] && { [ -z "${TRIAGE_CHECK:-}" ] || [ "${#branches[@]}" -lt 2 ]; }; then
      suite_broken_hold; return 0
    fi
    # IS MAIN ITSELF RED? Costs one scoped re-run on the rewound main; saves a gate per branch.
    if main_is_red; then
      for b in "${branches[@]}"; do echo "$b" >> "$QUEUE"; done
      printf '%s' "$gated_sig" > "$S/suite-broken"
      log "TRIAGE: MAIN IS RED on: $MAIN_RED_WHAT -> batch re-queued in order, NOT isolated; queue the fix"
      echo "$(date '+%m-%d %H:%M')  (bulk)  $tickets  MAIN_RED - $MAIN_RED_WHAT fail(s) on main itself; fix main, the batch is re-queued behind the fix" >> "$NEEDS"
      notify_coordinator "main itself fails $MAIN_RED_WHAT - the batch ($tickets) is re-queued and held; queue a fix for main." "main is red - fix for main needed"
      rm -f "$BULKMARK"
      return 0
    fi
    # A BATCH OF ONE (2026-09-26 00:50: task-t1009, the only branch of five that merged, was re-gated alone - a
    # 30-40 min full gate - to re-prove a red the triage had already run alone on base + it, with main green on
    # base). Blame it as the bisect blames its culprit, on the same evidence: red on base + it ALONE twice (the
    # triage's run, then one bisect_red probe - one red run cannot tell a defect from a test flaky even alone).
    # A green or given-up probe blames nobody: isolation decides, as before.
    if [ "${TRIAGE_KIND:-test}" = "test" ] && [ "${TRIAGE_ALONE_FIRST:-0}" = 1 ] \
       && { [ -n "${TRIAGE_FILTER:-}" ] || [ -n "${TRIAGE_SPECS:-}" ]; } && [ "${#branches[@]}" -eq 1 ]; then
      local culprit=${gated[0]%%=*} tip=${gated[0]#*=} side r
      side=$(main_side_of "$culprit" | sed 's/^main-side //')
      if [ -n "$side" ]; then
        log "TRIAGE: $culprit is red alone, but $(echo $side) is main-side -> no blame here; isolation decides"
      elif bisect_red "$base" "${gated[0]}"; r=$?; [ "$r" != 0 ]; then
        log "TRIAGE: $culprit, the batch's only merged branch: the confirming run alone on base + it $([ "$r" = 1 ] && echo "was green" || echo "gave up") -> no blame here; isolation decides"
      else
        rm -f "$S/isolate-remaining"
        record_attempt "$culprit" "$tip"
        log "GATE FAILED $culprit (the batch's only merged branch: red ALONE twice on base + it, green on base: $(echo ${TRIAGE_FILTER:-${TRIAGE_SPECS:-}})) -> abort + flag for AI"
        echo "$(date '+%m-%d %H:%M')  $culprit  $(ticket_of "$culprit")  GATE_FAIL" >> "$NEEDS"
        notify_coordinator "$(ticket_of "$culprit") ($culprit) FAILED the merge gate (the batch's only merged branch): $(echo ${TRIAGE_SPECS:-} ${TRIAGE_FILTER:-} | cut -c1-200)" "gate failed - fix run"
        rm -f "$BULKMARK"
        return 0
      fi
      bulk_dirty_stop && return 0
    fi
    # Only for a red that failed alone on its FIRST isolated run: one that passed alone and then
    # failed is flaky even alone, and a bisection over it would blame whichever branch it ended on.
    # A CHECK red is deterministic (a compile error, not a load-sensitive test), so it bisects by the check itself.
    if { { [ "${TRIAGE_KIND:-test}" = "test" ] && [ "${TRIAGE_ALONE_FIRST:-0}" = 1 ] \
           && { [ -n "${TRIAGE_FILTER:-}" ] || [ -n "${TRIAGE_SPECS:-}" ]; }; } \
         || { [ "${TRIAGE_KIND:-test}" = "suite" ] && [ -n "${TRIAGE_CHECK:-}" ]; }; } && [ "${#branches[@]}" -ge 2 ]; then
      # The bulk gate's own end line first, so hkpy.flow / cycletime close THIS gate here; the probes
      # below are not gates and print no `gate: … took` lines.
      log "BULK gate FAILED -> rewound to $base; the batch introduced it - bisecting before any isolate. BISECT: ${#branches[@]} branches, by $(echo ${TRIAGE_FILTER:-${TRIAGE_SPECS:-just ${TRIAGE_CHECK:-}}}) alone (instead of ${#branches[@]} serial gates)"
      local tips=("${gated[@]}") b culprit tip side=""
      culprit=$(bisect_culprit "$base" "${tips[@]}"); tip=${culprit#*=}; culprit=${culprit%%=*}
      bulk_dirty_stop && return 0
      # The single-branch path's main-side question (process): a spec failing alone on 2+ other
      # branches within a day is main's intermittent defect - then no blame here; isolation decides.
      [ -n "$culprit" ] && side=$(main_side_of "$culprit" | sed 's/^main-side //')
      [ -n "$side" ] && { log "BISECT: $culprit is red alone, but $(echo $side) is main-side -> no blame here"; culprit=""; }
      if [ -n "$culprit" ]; then
        local others=(); for b in "${branches[@]}"; do [ "$b" != "$culprit" ] && others+=("$b"); done
        { printf '%s\n' "${others[@]}"; cat "$QUEUE" 2>/dev/null; } > "$QUEUE.tmp" && mv "$QUEUE.tmp" "$QUEUE"
        rm -f "$S/isolate-remaining"
        record_attempt "$culprit" "$tip"
        log "GATE FAILED $culprit (bisected: red ALONE twice on base + it, green on base: $(echo ${TRIAGE_FILTER:-${TRIAGE_SPECS:-just ${TRIAGE_CHECK:-}}})) -> abort + flag for AI; ${#others[@]} other(s) re-queued first as one batch"
        echo "$(date '+%m-%d %H:%M')  $culprit  $(ticket_of "$culprit")  GATE_FAIL" >> "$NEEDS"
        notify_coordinator "$(ticket_of "$culprit") ($culprit) FAILED the merge gate (bisected from the batch): $(echo ${TRIAGE_SPECS:-} ${TRIAGE_FILTER:-} ${TRIAGE_CHECK:+just $TRIAGE_CHECK} | cut -c1-200)" "gate failed - fix run"
        rm -f "$BULKMARK"
        return 0
      fi
      rm -f "$S/isolate-remaining"
      # NO CULPRIT (supervisor, 2026-09-25 10:44: a both-ways flake sent 12 branches, six of them proven green, through
      # serial gates - 0 landings for an hour). The first time, the batch goes back FIRST as one batch and gates in full
      # again: proven green, then unprobed, then the last red subset. The record is every branch=sha so re-queued,
      # appended, never the batch signature (a re-queued batch picks up new branches, so that key re-queued a real pair
      # interaction forever) and never overwritten (a flaky red on another part of the batch re-queued it again): a
      # later no-culprit red subset sharing ANY recorded branch=sha isolates, as before - each branch=sha gets at most
      # one re-queue. No attempt is charged. A stale line (a landed or moved branch) only makes isolation sooner.
      local reds greens pg="" up="" rs="" t again="" why="no single branch confirmed red alone"
      [ -n "$side" ] && why="red alone, but $(echo $side) is main-side"
      grep -q '^gave-up' "$S/bisect-facts" 2>/dev/null && why="the bisect gave up"
      # A CHECK red the bisect could not probe through is still main+batch broken: held as before, never re-queued blind.
      if [ -n "${TRIAGE_CHECK:-}" ] && [ "$why" = "the bisect gave up" ]; then suite_broken_hold; return 0; fi
      reds=$(sed -n 's/^red //p' "$S/bisect-facts" 2>/dev/null | tail -1); greens=$(sed -n 's/^green //p' "$S/bisect-facts" 2>/dev/null)
      for t in $reds; do grep -qxF "$t" "$S/bisect-no-culprit" 2>/dev/null && again=1; done
      # Each half green, the whole red: a semantic conflict between branches (t844 x t989, 2026-09-25) - name them.
      [ -n "${TRIAGE_CHECK:-}" ] && echo "$(date '+%m-%d %H:%M')  (bulk)  $tickets  CHECK_PAIR - just $TRIAGE_CHECK red on base + $(for t in $reds; do printf '%s ' "${t%%=*}"; done)together, no branch red alone: a conflict inside that set; $([ -n "$again" ] && echo "isolating now" || echo "re-queued once, the next such red isolates")" >> "$NEEDS"
      if [ -z "$again" ]; then
        for b in "${branches[@]}"; do
          if printf '%s\n' ${reds} | grep -q "^$b="; then rs="$rs $b"
          elif printf '%s\n' ${greens} | grep -q "^$b="; then pg="$pg $b"
          else up="$up $b"; fi
        done
        { printf '%s\n' $pg $up $rs; cat "$QUEUE" 2>/dev/null; } > "$QUEUE.tmp" && mv "$QUEUE.tmp" "$QUEUE"
        printf '%s\n' "${gated[@]}" >> "$S/bisect-no-culprit"
        log "BISECT: no culprit blamed ($why) -> re-queued first as one batch (proven green:${pg:- none}, unprobed:${up:- none}, last red subset:${rs:- none}); a no-culprit red sharing any of these branch tips isolates"
        if [ "$(git -C "$REPO" rev-parse HEAD)" = "$base" ]; then rm -f "$BULKMARK"; fi
        return 0
      fi
      rm -f "$S/bisect-no-culprit"
      log "BISECT: no culprit blamed ($why) -> isolate by merging each individually"
    else
      log "BULK gate FAILED -> rewound to $base; isolate by merging each individually"
    fi
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

if [ ! -d "$CARGO_TARGET_DIR" ] && [ -d "$REPO/target" ]; then
  # Cloned to .tmp and moved into place: a half-copied dir would otherwise never be reseeded (review).
  rm -rf "$CARGO_TARGET_DIR.tmp"
  if cp -c -R -p "$REPO/target" "$CARGO_TARGET_DIR.tmp" 2>>"$LOG" && mv "$CARGO_TARGET_DIR.tmp" "$CARGO_TARGET_DIR"; then
    log "STARTUP: seeded the gate's target dir $CARGO_TARGET_DIR as a clone of $REPO/target"
  else
    rm -rf "$CARGO_TARGET_DIR.tmp"; log "STARTUP: WARN could not seed $CARGO_TARGET_DIR - the first gate builds cold"
  fi
fi
log "GATE TARGET: $CARGO_TARGET_DIR (main's target/ is the workers' clone source and is not rebuilt by gates)"
log "=== merge-runner up (DRY_RUN=$DRY_RUN, bulk mode); watching $QUEUE ==="
# What this process is actually running with - `just knobs show` reads it back as "effective".
log "KNOBS: WORKER_DRAIN_MAX=$WORKER_DRAIN_MAX FOREIGN_DRAIN_MAX=$FOREIGN_DRAIN_MAX BULK_MAX=$BULK_MAX GATE_TIMEOUT=$GATE_TIMEOUT MAX_ATTEMPTS=$MAX_ATTEMPTS FLAKE_SOLO_ONE=${FLAKE_SOLO_ONE:-0} GATE_TIERS=$GATE_TIERS RC_HOUR=$RC_HOUR"
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
# An isolation or a single merge this runner was killed in: those branches were in no queue - put them
# back, so a restart never loses them (and the queue-depth count never shows them as phantoms).
for f in "$S/isolate-remaining" "$S/merging-now"; do
  [ -s "$f" ] || { rm -f "$f"; continue; }
  sleft=$(tr -s ' \n' ' ' < "$f"); for b in $sleft; do echo "$b" >> "$QUEUE"; done
  rm -f "$f"; log "STARTUP: re-queued what a killed run was still holding ($(basename "$f")): $sleft"
done
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
  # RESTART ON REQUEST (pipeline manager, 2026-09-23). A landed runner change only takes effect on a
  # restart, and a restart is only safe BETWEEN gates - but with the queue non-empty the runner goes
  # straight from one gate to the next, so "wait for a gap" meant hours (three restarts were needed
  # on 2026-09-23 alone; the flake-acceptance rule waited on one). `$S/merge-runner-restart` (its
  # content is the reason, logged) is honoured HERE, the only point in the loop with no gate
  # running and no merge staged, by re-executing the repo's copy of this script in place.
  if [ -e "$S/merge-runner-restart" ] && [ ! -e "$S/bulk-in-progress" ] && [ ! -e "$REPO/.git/MERGE_HEAD" ]; then
    why=$(head -c 200 "$S/merge-runner-restart" 2>/dev/null | tr '\n' ' '); rm -f "$S/merge-runner-restart"
    log "RESTART: requested (${why:-no reason given}) - re-executing $REPO/ops/merge-runner.sh between gates"
    exec bash "$REPO/ops/merge-runner.sh"
  fi
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
    hold_until=$(sed -n 's/^until=//p' "$S/hold" | head -1); hold_since=$(sed -n 's/^since=//p' "$S/hold" | head -1)
    hold_why=$(sed -n 's/^why=//p' "$S/hold" | head -1); now_s=$(date +%s)
    # The READER enforces the bound too (review, 2026-09-23): a hand-written marker with a
    # non-numeric or far-off `until=` is capped at 30 minutes from `since=` (or from now), so
    # "bounded, auto-expiring" is a property of the runner and not only of `just hold`.
    case "$hold_until" in ''|*[!0-9]*) hold_until=0 ;; esac
    case "$hold_since" in ''|*[!0-9]*) hold_since=$now_s ;; esac
    [ "$hold_until" -gt $(( hold_since + 1800 )) ] && { hold_until=$(( hold_since + 1800 )); log "HOLD: marker asked for more than 30 min - capped at $(date -r "$hold_until" '+%H:%M' 2>/dev/null || echo "$hold_until")"; }
    if [ "$now_s" -ge "$hold_until" ]; then
      rm -f "$S/hold"; log "HOLD expired ($hold_why) - resuming"
      hold_event expired "$hold_why"
      # Invariant 6's second alert: the hold ran out with work already waiting behind it.
      [ -n "$queued" ] && alert amber "hold reached its expiry with work waiting" "$hold_why - expired with queued: $(printf '%s' "$queued" | tr '\n' ' ' | cut -c1-80)" --key "hold-expired-queued"
    elif [ -n "$queued" ]; then
      rm -f "$S/hold"; log "HOLD ended at the first queued branch ($hold_why): $(printf '%s' "$queued" | tr '\n' ' ' | cut -c1-80)"
      hold_event ended-by-queue "$hold_why"
      alert amber "hold ended by queued work" "$hold_why - a branch arrived; the runner resumed. Blocked minutes are charged to the open experiment." --key "hold-ended"
    else
      [ -z "${HOLD_SAID:-}" ] && { log "HOLD: merge queue held until $(date -r "$hold_until" '+%H:%M' 2>/dev/null || echo "$hold_until") - $hold_why"; HOLD_SAID=1; }
      sleep 8; continue
    fi
  fi
  HOLD_SAID=""
  # MAIN_DIRTY (reset_to_base): main carries an ungated commit the runner could not remove. No gate, merge or RC on it;
  # the alert and the attention line went out when it was written. A person resets main and removes the marker.
  if [ -e "$S/main-dirty" ]; then
    [ -z "${DIRTY_SAID:-}" ] && { log "HOLD: MAIN_DIRTY - $(tr '\n' ' ' < "$S/main-dirty") - no gate or merge until a person resets main and removes $S/main-dirty"; DIRTY_SAID=1; }
    sleep 8; continue
  fi
  DIRTY_SAID=""
  if rc_due && [ ! -e "$BULKMARK" ] && main_ready && workers_drained; then run_rc; continue; fi
  if [ -n "$queued" ] && main_ready && workers_drained; then
    # keep only branches that still exist and are ahead of main
    ready=$(ready_filter $queued)
    # Dedupe, first occurrence wins: a branch appended more than once (each new tip re-queues the
    # same name) took one BULK_MAX slot per copy - `task-alerts` x4 pushed gate-diag, spec-waits
    # and watchdog out of the 03:12 batch on 2026-09-23.
    ready=$(printf '%s\n' $ready | awk '!seen[$0]++' | tr '\n' ' ')
    # drop the non-comment lines we're about to act on (keep comments); transient branches get requeued
    grep -E '^\s*#' "$QUEUE" > "$QUEUE.tmp" 2>/dev/null || true; mv "$QUEUE.tmp" "$QUEUE" 2>/dev/null || true
    # A task-pm-* branch the scope check held stays queued (it merges by itself once released).
    [ -s "$S/pm-held" ] && cat "$S/pm-held" >> "$QUEUE"
    # A branch whose red main shares (main_is_red in process) waits for main to change - any landing
    # releases it for one more gate. Before the CHEAP FIRST split, so a fix for main still gates.
    if [ -s "$S/main-red-parked" ]; then
      # Keyed on main's HEAD AND the branch's tip: a fix pushed to the branch itself releases it too.
      parked=$(while read -r pb ph pt; do
        [ "$ph" = "$(git -C "$REPO" rev-parse HEAD)" ] && [ "$pt" = "$(git -C "$REPO" rev-parse --verify --quiet "$pb")" ] && echo "$pb"
      done < "$S/main-red-parked")
      if [ -z "$parked" ]; then
        rm -f "$S/main-red-parked"; PARK_SAID=""; log "PARK: main moved - parked branches gate again"
      else
        keep=""
        for b in $ready; do
          if printf '%s\n' $parked | grep -qx "$b"; then echo "$b" >> "$QUEUE"; else keep="$keep $b"; fi
        done
        ready="${keep# }"
        [ -z "${PARK_SAID:-}" ] && { log "PARK: $(echo $parked) wait for main to move (main is red on their red)"; PARK_SAID=1; }
        [ -z "$ready" ] && { sleep 8; continue; }
      fi
    fi
    # CHEAP FIRST (user, 2026-09-24: a py+ops or ui-only branch queued behind a batch "should be the
    # very next attempt, alone, so it lands in ~2 min after the batch rather than joining the next
    # full one"). hkpy.gatepri classifies each branch with the gate's own rule; the cheap ones go now,
    # the rest back in order. It narrows no gate: the attempt is gated by `just gate` as always. Any
    # failure to classify prints nothing and the batch is formed as before. It runs BEFORE the
    # SUITE_BROKEN hold below, so the hold compares the batch that will actually gate (review: after
    # it, a red cheap batch's signature never matched the mixed queue, and it re-gated forever).
    if [ "$(printf '%s\n' $ready | grep -c .)" -ge 2 ]; then
      part=$(cd "$REPO" && uv run --locked --project py python -m hkpy.gatepri main $ready 2>/dev/null)
      cheap=$(printf '%s\n' "$part" | sed -n 's/^cheap: //p'); rest=$(printf '%s\n' "$part" | sed -n 's/^rest: //p')
      if [ -n "$cheap" ] && [ -n "$rest" ]; then
        log "CHEAP FIRST: $cheap goes alone before $rest ($(printf '%s\n' "$part" | sed -n 's/^classes: //p'))"
        for b in $rest; do echo "$b" >> "$QUEUE"; done
        ready="$cheap"
      fi
    fi
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
      echo "$1" > "$S/merging-now"; process "$1" || echo "$1" >> "$QUEUE"; rm -f "$S/merging-now"
    elif [ "$#" -ge 2 ]; then
      BULK_MERGED_LIST=""
      if ! try_bulk "$@"; then
        # Isolate only what the batch actually merged. A branch try_bulk SKIPPED conflicted, and
        # is already flagged in merge-needs-attention.txt; sending it round again just conflicts
        # a second time and writes a duplicate flag.
        isolate="${BULK_MERGED_LIST:-$*}"
        log "falling back to individual gates for: $isolate"
        MAIN_RED_STOP=""
        # The isolation's remainder lives only in this loop; written out so "branches not yet on
        # main" (hkpy.flow.queue_waiting, /flow's queue depth) counts it (user, 2026-09-24 17:02).
        rest="$isolate"
        for b in $isolate; do
          rest=$(printf '%s\n' $rest | grep -vx "$b" | tr '\n' ' '); printf '%s %s\n' "$b" "$rest" > "$S/isolate-remaining"
          # Main is red: the rest would each fail the same way - back to the queue, whose next batch
          # meets the batch path's MAIN IS RED hold.
          if [ -n "$MAIN_RED_STOP" ]; then echo "$b" >> "$QUEUE"; continue; fi
          process "$b" || echo "$b" >> "$QUEUE"
        done
        rm -f "$S/isolate-remaining"
      fi
    fi
  fi
  sleep 8
done
