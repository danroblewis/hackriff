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
MAX_ATTEMPTS=${MAX_ATTEMPTS:-2}
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
notify_coordinator(){ tmux has-session -t dev 2>/dev/null || return 0; tmux send-keys -t dev -l "MERGE-RUNNER: $1 See $NEEDS; fix it, then re-queue the branch." 2>/dev/null; sleep 1; tmux send-keys -t dev Enter 2>/dev/null; }
# Edge-triggered wake on a SUCCESSFUL merge: a clean merge drains the queue and may unblock
# dependent tickets, but nothing else pings the coordinator for it (task-completions and the
# failure ping above cover their cases). Without this, the coordinator can sit idle after a
# green merge with startable work undone. It says "reconcile", never a computed to-do list:
# the coordinator's `just reconcile` is the single source of truth, and any list we pasted here
# would be stale by the time it acts.
notify_ok(){ tmux has-session -t dev 2>/dev/null || return 0; tmux send-keys -t dev -l "MERGE-RUNNER: $1 Reconcile, then fill the builder cap from startable work." 2>/dev/null; sleep 1; tmux send-keys -t dev Enter 2>/dev/null; }
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
  if just gate-merge >>"$LOG" 2>&1; then
    git commit -m "Merge $ticket ($branch): gate passed (automated merge, no AI)" >>"$LOG" 2>&1
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
  ( cd "$REPO" && just gate --base "$base" ) >>"$LOG" 2>&1; rc=$?
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
self_version
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
  if [ -n "$queued" ] && main_ready; then
    # keep only branches that still exist and are ahead of main
    ready=$(ready_filter $queued)
    # drop the non-comment lines we're about to act on (keep comments); transient branches get requeued
    grep -E '^\s*#' "$QUEUE" > "$QUEUE.tmp" 2>/dev/null || true; mv "$QUEUE.tmp" "$QUEUE" 2>/dev/null || true
    set -- $ready
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
