#!/usr/bin/env bash
# heartbeat.sh — supervisor heartbeat backstop for the coordinator.
#
# The coordinator has NO autonomous timer: it wakes only on (a) a background agent/task
# completing and (b) a tmux ping (the merge-runner's success/failure notifications). That
# covers almost everything, but leaves one hole — the coordinator can end a turn IDLE with
# startable work undone and nothing scheduled to wake it. This is the level-triggered
# backstop for exactly that: a slow poll that pokes the coordinator ONLY when it is idle
# AND real work exists. The real-time waking is still done by events; this only catches the
# residual "idle-with-work" stall (a lapse in the coordinator's momentum, or a worker that
# died without emitting a completion).
#
# DESIGN: this script is deliberately dumb. It decides ONLY whether to fire; it never hands
# the coordinator a computed to-do list. The coordinator's `just reconcile` reads git +
# tasks.yaml + the queue and is the single source of truth — any list pasted here would be a
# stale snapshot by the time it acts (the merge-runner lands merges between our poll and its
# reconcile). So the poke says "reconcile and fill the cap"; the counts we include are an
# explicitly-labelled hint, not instructions.
#
# Watch:  tail -f $HACKRIFF_OPS/heartbeat.log
# Stop:   pkill -f ops/heartbeat.sh
set -uo pipefail
REPO=/Users/daniellewis/hackriff
S="${HACKRIFF_OPS:-$HOME/.hackriff-ops}"; mkdir -p "$S"
SESSION="${HEARTBEAT_SESSION:-dev}"          # the coordinator's tmux session
INTERVAL="${HEARTBEAT_INTERVAL:-600}"        # 10 min: slow, because events do the real work
LOG="$S/heartbeat.log"
QUEUE="$S/merge-queue.txt"
log(){ echo "[$(date '+%m-%d %H:%M:%S')] $*" | tee -a "$LOG"; }

# idle == the coordinator's pane is at the prompt with no active turn or command.
# "esc to interrupt" is shown ONLY while a turn/foreground-command runs (including while it
# waits on background agents), so its ABSENCE is the reliable idle signal. Stale agent rows
# can linger in the pane after a turn ends, but "esc to interrupt" does not — which is why we
# key off it and not off the agent rows.
coord_idle(){
  tmux has-session -t "$SESSION" 2>/dev/null || return 1
  local pane; pane=$(tmux capture-pane -t "$SESSION" -p 2>/dev/null) || return 1
  # BUSY if any of: an active turn/foreground command ("esc to interrupt"), the coordinator
  # blocked waiting on background agents ("Waiting for N background agents" — note this state
  # shows NO "esc to interrupt", so it must be matched explicitly), or a running shell command.
  printf '%s' "$pane" | grep -qE 'esc to interrupt|Waiting for [0-9]+ background|Running [0-9]+ shell command' && return 1
  return 0
}

# how many `todo` tickets have every dependency already `done` (i.e. can start right now).
# This gates whether to poke at all — it is NOT sent to the coordinator as a work list.
startable_count(){
  python3 - "$REPO/docs/tasks.yaml" <<'PY' 2>/dev/null || echo 0
import yaml,sys
try: d=yaml.safe_load(open(sys.argv[1]))
except Exception: print(0); sys.exit()
tasks=d if isinstance(d,list) else d.get('tasks',d)
if isinstance(tasks,dict): tasks=list(tasks.values())
byid={t['id']:t for t in tasks if isinstance(t,dict) and 'id' in t}
done={i for i,t in byid.items() if t.get('status')=='done'}
def deps(t):
    x=t.get('depends_on') or t.get('deps') or t.get('after') or []
    return x if isinstance(x,list) else [x]
print(sum(1 for t in tasks if isinstance(t,dict) and t.get('status')=='todo'
         and all(dd in done for dd in deps(t))))
PY
}

poke(){
  tmux send-keys -t "$SESSION" -l "$1" 2>/dev/null; sleep 1
  tmux send-keys -t "$SESSION" Enter 2>/dev/null; sleep 1
  tmux send-keys -t "$SESSION" Enter 2>/dev/null   # a 2nd Enter submits if a long line got buffered as a paste; harmless on an empty prompt
}

log "=== heartbeat up (interval ${INTERVAL}s, session '$SESSION') ==="
consec=0
while true; do
  sleep "$INTERVAL"
  # require idle across two samples ~6s apart so a momentary between-turns gap is not mistaken for a stall
  coord_idle || { log "tick: busy — skip"; consec=0; continue; }
  sleep 6
  coord_idle || { log "tick: busy (settled) — skip"; consec=0; continue; }

  n=$(startable_count | tr -dc '0-9'); n=${n:-0}
  q=$(grep -vcE '^[[:space:]]*(#|$)' "$QUEUE" 2>/dev/null || echo 0)
  if [ "$n" -gt 0 ]; then
    consec=$((consec+1))
    log "tick: IDLE with $n startable (queue=$q) — poke (#$consec since last activity)"
    poke "SUPERVISOR HEARTBEAT: you look idle at the prompt. \`just reconcile\`, then fill the builder cap from startable work and clear any needs-attention. (~$n startable, $q queued — a stale hint, verify via reconcile.)"
    if [ "$consec" -ge 3 ]; then
      log "!! WARNING: coordinator still idle-with-work after $consec consecutive pokes — may be wedged; a human should look."
    fi
  else
    log "tick: idle but 0 startable (queue=$q) — nothing to do"
    consec=0
  fi
done
