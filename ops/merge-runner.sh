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
DRY_RUN=${DRY_RUN:-0}
touch "$QUEUE" "$NEEDS" "$DONELOG"

log(){ echo "[$(date '+%m-%d %H:%M:%S')] $*" | tee -a "$LOG"; }
notify_coordinator(){ tmux has-session -t dev 2>/dev/null || return 0; tmux send-keys -t dev -l "MERGE-RUNNER: $1 See $NEEDS; fix it, then re-queue the branch." 2>/dev/null; sleep 1; tmux send-keys -t dev Enter 2>/dev/null; }
ticket_of(){ echo "$1" | sed -E 's/^task-t0*([0-9]+)$/T-\1/I'; }
worktree_of(){ git -C "$REPO" worktree list --porcelain \
  | awk -v b="refs/heads/$1" '/^worktree /{p=substr($0,10)} /^branch /{if(substr($0,8)==b) print p}'; }

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
    echo "$(date '+%m-%d %H:%M')  $branch  $ticket  MERGED" >> "$DONELOG"
    local wt; wt=$(worktree_of "$branch")
    if [ -n "$wt" ] && [ "$(cd "$wt" && pwd -P)" != "$(cd "$REPO" && pwd -P)" ]; then
      git worktree remove "$wt" --force 2>>"$LOG" && log "worktree removed: $wt"
    fi
  else
    git merge --abort 2>/dev/null || true
    log "GATE FAILED $branch -> abort + flag for AI"
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

# BULK: octopus-merge all branches, gate ONCE, commit only if green.
# 0 = all merged; 1 = caller should fall back to merging each individually.
try_bulk(){
  local branches=("$@") tickets="" b wt
  for b in "${branches[@]}"; do tickets="$tickets $(ticket_of "$b")"; done
  tickets="${tickets# }"
  log "BULK attempt (${#branches[@]}): ${branches[*]}"
  if [ "$DRY_RUN" = "1" ]; then log "DRY-RUN would bulk-merge: $tickets"; return 0; fi
  if ! git -C "$REPO" merge --no-commit "${branches[@]}" >>"$LOG" 2>&1; then
    git -C "$REPO" merge --abort 2>/dev/null || true
    log "BULK conflict across branches -> fall back to individual"; return 1
  fi
  log "BULK gate (just gate-merge over the combined index; may take 15-25 min)…"
  if ( cd "$REPO" && just gate-merge ) >>"$LOG" 2>&1; then
    git -C "$REPO" commit -m "Merge $tickets: bulk (${#branches[@]} branches), gate passed (automated, no AI)" >>"$LOG" 2>&1
    log "BULK MERGED ✓ $tickets"
    for b in "${branches[@]}"; do
      echo "$(date '+%m-%d %H:%M')  $b  $(ticket_of "$b")  MERGED(bulk)" >> "$DONELOG"
      wt=$(worktree_of "$b")
      [ -n "$wt" ] && [ "$wt" != "$REPO" ] && git -C "$REPO" worktree remove "$wt" --force 2>>"$LOG" && log "worktree removed: $wt"
    done
    return 0
  fi
  git -C "$REPO" merge --abort 2>/dev/null || true
  log "BULK gate FAILED -> isolate by merging each individually"; return 1
}

log "=== merge-runner up (DRY_RUN=$DRY_RUN, bulk mode); watching $QUEUE ==="
while true; do
  # read every queued (non-comment) branch, in order
  queued=$(grep -vE '^\s*(#|$)' "$QUEUE" 2>/dev/null | tr -d '[:space:]' | grep -v '^$' || true)
  if [ -n "$queued" ] && main_ready; then
    # keep only branches that still exist and are ahead of main
    ready=$(ready_filter $queued)
    # drop the non-comment lines we're about to act on (keep comments); transient branches get requeued
    grep -E '^\s*#' "$QUEUE" > "$QUEUE.tmp" 2>/dev/null || true; mv "$QUEUE.tmp" "$QUEUE" 2>/dev/null || true
    set -- $ready
    if [ "$#" -eq 1 ]; then
      process "$1" || echo "$1" >> "$QUEUE"
    elif [ "$#" -ge 2 ]; then
      if ! try_bulk "$@"; then
        log "falling back to individual gates for: $*"
        for b in "$@"; do process "$b" || echo "$b" >> "$QUEUE"; done
      fi
    fi
  fi
  sleep 8
done
