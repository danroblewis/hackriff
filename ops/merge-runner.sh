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
    echo "$(date '+%m-%d %H:%M')  $branch  $ticket  CONFLICT" >> "$NEEDS"; return 0
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
    echo "$(date '+%m-%d %H:%M')  $branch  $ticket  GATE_FAIL" >> "$NEEDS"
  fi
  return 0
}

log "=== merge-runner up (DRY_RUN=$DRY_RUN); watching $QUEUE ==="
while true; do
  branch=$(grep -m1 -vE '^\s*(#|$)' "$QUEUE" 2>/dev/null | tr -d '[:space:]')
  if [ -n "$branch" ]; then
    # pop the first non-comment line matching this branch
    python3 - "$QUEUE" "$branch" <<'PY' 2>/dev/null || true
import sys
q,br=sys.argv[1],sys.argv[2]
lines=open(q).read().splitlines()
out=[]; dropped=False
for l in lines:
    if not dropped and l.strip()==br: dropped=True; continue
    out.append(l)
open(q,'w').write("\n".join(out)+("\n" if out else ""))
PY
    if ! process "$branch"; then
      echo "$branch" >> "$QUEUE"   # transient: requeue at end
      sleep 25
    fi
  fi
  sleep 8
done
